use std::{error::Error, fmt, num::NonZeroU32, path::Path};

use omap::{
    Code, NonNegativeF64, Omap,
    colors::{Cmyk, Rgb, SpotColor, SymbolColor},
    geo_types::Coord,
    objects::{AreaObject, BezierPath, BezierPolygon, BezierSegment, BezierString},
    symbols::{AreaSymbol, Element, PointSymbol},
};
use usvg::{BlendMode, FillRule, Node, Paint, PaintOrder};

const MM_PER_INCH: f64 = 25.4;
const DEFAULT_DPI: f64 = 96.0;
const TOPOLOGY_FLATTENING_TOLERANCE_MM: f64 = 0.0025;
const OPACITY_EPSILON: f64 = 1.0e-6;

/// Options controlling SVG parsing and OMAP creation.
#[derive(Debug, Clone)]
pub struct ConversionOptions {
    /// Name of the point symbol in Mapper.
    pub symbol_name: String,
    /// Numeric symbol code in Mapper.
    pub symbol_code: Code,
    /// Scale denominator stored on the otherwise empty map.
    pub map_scale: NonZeroU32,
    /// CSS pixel density used to resolve SVG physical units.
    pub dpi: f64,
    /// Optional physical width override for the SVG viewport.
    pub width_mm: Option<f64>,
}

impl ConversionOptions {
    /// Construct options with conventional defaults.
    pub fn new(symbol_name: impl Into<String>) -> Self {
        Self {
            symbol_name: symbol_name.into(),
            symbol_code: Code::new(900, 0, 0),
            map_scale: NonZeroU32::new(10_000).expect("10,000 is non-zero"),
            dpi: DEFAULT_DPI,
            width_mm: None,
        }
    }
}

/// The converted map and useful conversion statistics.
#[derive(Debug)]
pub struct Conversion {
    /// An otherwise empty OMAP document containing one point symbol.
    pub map: Omap,
    /// Physical SVG viewport width in millimetres.
    pub width_mm: f64,
    /// Physical SVG viewport height in millimetres.
    pub height_mm: f64,
    /// Number of SVG fill/stroke paint layers converted.
    pub paint_layers: usize,
    /// Number of area elements created inside the point symbol.
    pub area_elements: usize,
    /// Non-fatal SVG features that were approximated or skipped.
    pub warnings: Vec<String>,
}

/// A conversion failure with a user-facing explanation.
#[derive(Debug, Clone)]
pub struct ConversionError(String);

impl ConversionError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ConversionError {}

type Result<T> = std::result::Result<T, ConversionError>;

#[derive(Clone, Copy)]
struct CoordinateSystem {
    center_x_px: f64,
    center_y_px: f64,
    mm_per_px: f64,
}

#[derive(Debug)]
struct PaintLayer {
    color: usvg::Color,
    polygons: Vec<BezierPolygon>,
}

/// Convert SVG bytes into an empty map containing a single point symbol.
///
/// SVG shapes, transforms, CSS, primitive elements, strokes, dashes and text
/// outlines are resolved by `usvg`. OMAP has no direct equivalent for
/// gradients, transparency, clipping, masks, filters, blending, patterns, or
/// raster images. Unsupported effects are approximated or skipped and reported
/// in [`Conversion::warnings`].
pub fn convert_svg(
    svg_data: &[u8],
    resource_directory: Option<&Path>,
    options: &ConversionOptions,
) -> Result<Conversion> {
    validate_options(options)?;

    let mut svg_options = usvg::Options {
        dpi: options.dpi as f32,
        resources_dir: resource_directory.map(Path::to_path_buf),
        ..Default::default()
    };
    svg_options.fontdb_mut().load_system_fonts();

    let tree = usvg::Tree::from_data(svg_data, &svg_options)
        .map_err(|error| ConversionError::new(format!("could not parse SVG: {error}")))?;

    let viewport_width_px = f64::from(tree.size().width());
    let viewport_height_px = f64::from(tree.size().height());
    let mm_per_px = options
        .width_mm
        .map_or(MM_PER_INCH / options.dpi, |width| width / viewport_width_px);
    let coordinates = CoordinateSystem {
        center_x_px: viewport_width_px / 2.0,
        center_y_px: viewport_height_px / 2.0,
        mm_per_px,
    };

    let mut layers = Vec::new();
    let mut warnings = Vec::new();
    collect_group(tree.root(), coordinates, &mut layers, &mut warnings)?;
    let layers = merge_adjacent_layers(layers);

    let mut map = Omap::new(options.map_scale);
    let mut point_symbol = PointSymbol::new(options.symbol_code, &options.symbol_name);
    let mut area_elements = 0;

    // New SVG paint operations cover older ones. Inserting each new color at
    // priority zero gives it the corresponding higher OMAP drawing priority.
    for (layer_index, layer) in layers.iter().enumerate() {
        let hex = format!(
            "#{:02x}{:02x}{:02x}",
            layer.color.red, layer.color.green, layer.color.blue
        );
        let rgb: Rgb = hex
            .parse()
            .map_err(|error| ConversionError::new(format!("could not convert {hex}: {error}")))?;
        let mut color = SpotColor::new(
            format!("SVG layer {} ({hex})", layer_index + 1),
            format!("svg-layer-{}", layer_index + 1),
            Cmyk::from(rgb),
        );
        // SVG paint is opaque and replaces what is below it.
        color.knockout = true;
        let color_id = map
            .colors
            .insert(0, color)
            .map_err(|error| ConversionError::new(format!("could not add OMAP color: {error}")))?;

        for polygon in &layer.polygons {
            let area_symbol =
                AreaSymbol::new(Code::default(), "").with_color(SymbolColor::Color(color_id));
            point_symbol.elements.push(Element::Area {
                symbol: Box::new(area_symbol),
                object: Box::new(AreaObject::new_element(polygon.clone())),
            });
            area_elements += 1;
        }
    }

    map.symbols.add_point_symbol(point_symbol);
    map.validate()
        .map_err(|error| ConversionError::new(format!("created an invalid OMAP file: {error}")))?;

    Ok(Conversion {
        map,
        width_mm: viewport_width_px * mm_per_px,
        height_mm: viewport_height_px * mm_per_px,
        paint_layers: layers.len(),
        area_elements,
        warnings,
    })
}

fn merge_adjacent_layers(layers: Vec<PaintLayer>) -> Vec<PaintLayer> {
    let mut merged: Vec<PaintLayer> = Vec::with_capacity(layers.len());
    for mut layer in layers {
        if let Some(previous) = merged.last_mut()
            && previous.color == layer.color
        {
            previous.polygons.append(&mut layer.polygons);
        } else {
            merged.push(layer);
        }
    }
    merged
}

fn validate_options(options: &ConversionOptions) -> Result<()> {
    if !options.dpi.is_finite() || options.dpi <= 0.0 {
        return Err(ConversionError::new("DPI must be a positive finite number"));
    }
    if options
        .width_mm
        .is_some_and(|width| !width.is_finite() || width <= 0.0)
    {
        return Err(ConversionError::new(
            "the width override must be a positive finite number",
        ));
    }
    if options.symbol_name.trim().is_empty() {
        return Err(ConversionError::new("the symbol name cannot be empty"));
    }
    Ok(())
}

fn collect_group(
    group: &usvg::Group,
    coordinates: CoordinateSystem,
    layers: &mut Vec<PaintLayer>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let opacity = f64::from(group.opacity().get());
    if opacity <= OPACITY_EPSILON {
        return Ok(());
    }
    warn_if_transparent(opacity, "an SVG group", warnings);

    if group.blend_mode() != BlendMode::Normal {
        push_warning(
            warnings,
            format!(
                "SVG blend mode {:?} was ignored and rendered as normal paint",
                group.blend_mode()
            ),
        );
    }
    if group.clip_path().is_some() {
        push_warning(
            warnings,
            "an SVG clip path was ignored; its artwork was converted unclipped",
        );
    }
    if group.mask().is_some() {
        push_warning(
            warnings,
            "an SVG mask was ignored; its artwork was converted unmasked",
        );
    }
    if !group.filters().is_empty() {
        push_warning(
            warnings,
            "one or more SVG filters were ignored while converting their artwork",
        );
    }

    for node in group.children() {
        match node {
            Node::Group(child) => collect_group(child, coordinates, layers, warnings)?,
            Node::Path(path) if path.is_visible() => {
                collect_path(path, coordinates, layers, warnings)?;
            }
            Node::Path(_) => {}
            Node::Text(text) => {
                collect_group(text.flattened(), coordinates, layers, warnings)?;
            }
            Node::Image(_) => {
                push_warning(
                    warnings,
                    "a raster or embedded SVG image was skipped; convert it to paths to include it",
                );
            }
        }
    }
    Ok(())
}

fn collect_path(
    path: &usvg::Path,
    coordinates: CoordinateSystem,
    layers: &mut Vec<PaintLayer>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let operations = match path.paint_order() {
        PaintOrder::FillAndStroke => [
            fill_layer(path, coordinates, warnings)?,
            stroke_layer(path, coordinates, warnings)?,
        ],
        PaintOrder::StrokeAndFill => [
            stroke_layer(path, coordinates, warnings)?,
            fill_layer(path, coordinates, warnings)?,
        ],
    };
    layers.extend(operations.into_iter().flatten());
    Ok(())
}

fn fill_layer(
    path: &usvg::Path,
    coordinates: CoordinateSystem,
    warnings: &mut Vec<String>,
) -> Result<Option<PaintLayer>> {
    let Some(fill) = path.fill() else {
        return Ok(None);
    };
    let opacity = f64::from(fill.opacity().get());
    if opacity <= OPACITY_EPSILON {
        return Ok(None);
    }
    warn_if_transparent(opacity, path_description(path), warnings);
    let Some(color) = paint_color(fill.paint(), path_description(path), warnings) else {
        return Ok(None);
    };
    let polygons = path_to_polygons(path.data(), path.abs_transform(), coordinates, fill.rule())?;
    Ok((!polygons.is_empty()).then_some(PaintLayer { color, polygons }))
}

fn stroke_layer(
    path: &usvg::Path,
    coordinates: CoordinateSystem,
    warnings: &mut Vec<String>,
) -> Result<Option<PaintLayer>> {
    let Some(stroke) = path.stroke() else {
        return Ok(None);
    };
    let opacity = f64::from(stroke.opacity().get());
    if opacity <= OPACITY_EPSILON {
        return Ok(None);
    }
    warn_if_transparent(opacity, path_description(path), warnings);
    let Some(color) = paint_color(stroke.paint(), path_description(path), warnings) else {
        return Ok(None);
    };
    let resolution_scale = path
        .abs_transform()
        .get_scale()
        .0
        .max(path.abs_transform().get_scale().1)
        .max(1.0);
    let mut stroke_style = stroke.to_tiny_skia();
    let dashed_path = stroke_style
        .dash
        .take()
        .map(|dash| {
            path.data().dash(&dash, resolution_scale).ok_or_else(|| {
                ConversionError::new(format!(
                    "could not apply the dash pattern of {}",
                    path_description(path)
                ))
            })
        })
        .transpose()?;
    let stroke_source = dashed_path.as_ref().unwrap_or_else(|| path.data());
    let outline = stroke_source
        .stroke(&stroke_style, resolution_scale)
        .ok_or_else(|| {
            ConversionError::new(format!(
                "could not expand the stroke of {} into an outline",
                path_description(path)
            ))
        })?;
    let polygons = path_to_polygons(
        &outline,
        path.abs_transform(),
        coordinates,
        FillRule::NonZero,
    )?;
    Ok((!polygons.is_empty()).then_some(PaintLayer { color, polygons }))
}

fn path_description(path: &usvg::Path) -> &str {
    if path.id().is_empty() {
        "an unnamed SVG path"
    } else {
        path.id()
    }
}

fn warn_if_transparent(opacity: f64, description: &str, warnings: &mut Vec<String>) {
    if (opacity - 1.0).abs() > OPACITY_EPSILON {
        push_warning(
            warnings,
            format!("{description} uses opacity {opacity:.3}; it was made fully opaque"),
        );
    }
}

fn paint_color(
    paint: &Paint,
    description: &str,
    warnings: &mut Vec<String>,
) -> Option<usvg::Color> {
    match paint {
        Paint::Color(color) => Some(*color),
        Paint::LinearGradient(gradient) => {
            let color = average_gradient_color(gradient.stops());
            push_warning(
                warnings,
                format!(
                    "{description} uses a linear gradient; it was replaced with {}",
                    color_hex(color)
                ),
            );
            warn_for_transparent_stops(gradient.stops(), description, warnings);
            Some(color)
        }
        Paint::RadialGradient(gradient) => {
            let color = average_gradient_color(gradient.stops());
            push_warning(
                warnings,
                format!(
                    "{description} uses a radial gradient; it was replaced with {}",
                    color_hex(color)
                ),
            );
            warn_for_transparent_stops(gradient.stops(), description, warnings);
            Some(color)
        }
        Paint::Pattern(_) => {
            push_warning(
                warnings,
                format!("{description} uses an SVG paint pattern; that paint was skipped"),
            );
            None
        }
    }
}

fn warn_for_transparent_stops(stops: &[usvg::Stop], description: &str, warnings: &mut Vec<String>) {
    if stops
        .iter()
        .any(|stop| stop.opacity().get() < 1.0 - OPACITY_EPSILON as f32)
    {
        push_warning(
            warnings,
            format!("{description} has transparent gradient stops; they were made fully opaque"),
        );
    }
}

fn average_gradient_color(stops: &[usvg::Stop]) -> usvg::Color {
    let Some(first) = stops.first() else {
        return usvg::Color::black();
    };

    let mut red = f64::from(first.color().red) * f64::from(first.offset().get());
    let mut green = f64::from(first.color().green) * f64::from(first.offset().get());
    let mut blue = f64::from(first.color().blue) * f64::from(first.offset().get());

    for pair in stops.windows(2) {
        let left = pair[0];
        let right = pair[1];
        let width = f64::from(right.offset().get() - left.offset().get());
        red += width * f64::from(u16::from(left.color().red) + u16::from(right.color().red)) / 2.0;
        green +=
            width * f64::from(u16::from(left.color().green) + u16::from(right.color().green)) / 2.0;
        blue +=
            width * f64::from(u16::from(left.color().blue) + u16::from(right.color().blue)) / 2.0;
    }

    let last = stops.last().expect("a first stop implies a last stop");
    let remaining = 1.0 - f64::from(last.offset().get());
    red += f64::from(last.color().red) * remaining;
    green += f64::from(last.color().green) * remaining;
    blue += f64::from(last.color().blue) * remaining;

    usvg::Color::new_rgb(
        red.round().clamp(0.0, 255.0) as u8,
        green.round().clamp(0.0, 255.0) as u8,
        blue.round().clamp(0.0, 255.0) as u8,
    )
}

fn color_hex(color: usvg::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red, color.green, color.blue)
}

fn push_warning(warnings: &mut Vec<String>, warning: impl Into<String>) {
    let warning = warning.into();
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn path_to_polygons(
    path: &usvg::tiny_skia_path::Path,
    transform: usvg::Transform,
    coordinates: CoordinateSystem,
    fill_rule: FillRule,
) -> Result<Vec<BezierPolygon>> {
    let paths = split_contours(path, transform, coordinates)?;
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut contours = paths
        .into_iter()
        .map(|path| {
            let flattened = path
                .flatten(NonNegativeF64::clamped_from(
                    TOPOLOGY_FLATTENING_TOLERANCE_MM,
                ))
                .map_err(|error| {
                    ConversionError::new(format!(
                        "could not flatten a path for ring classification: {error}"
                    ))
                })?;
            let points = flattened.geometry().0.clone();
            let signed_area = signed_area(&points);
            Ok(Contour {
                path,
                points,
                signed_area,
                parent: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    contours.retain(|contour| contour.signed_area.abs() > f64::EPSILON);
    classify_parents(&mut contours);

    let boundaries = contours
        .iter()
        .enumerate()
        .map(|(index, contour)| boundary_kind(index, contour, &contours, fill_rule))
        .collect::<Vec<_>>();

    let mut polygons = Vec::new();
    for (outer_index, boundary) in boundaries.iter().enumerate() {
        if *boundary != Boundary::Outer {
            continue;
        }
        let holes = boundaries
            .iter()
            .enumerate()
            .filter(|(index, kind)| {
                **kind == Boundary::Hole
                    && closest_outer_ancestor(*index, &contours, &boundaries) == Some(outer_index)
            })
            .map(|(index, _)| contours[index].path.clone())
            .collect();
        polygons.push(
            BezierPolygon::new(contours[outer_index].path.clone(), holes).map_err(|error| {
                ConversionError::new(format!("could not create an OMAP polygon: {error}"))
            })?,
        );
    }
    Ok(polygons)
}

#[derive(Debug)]
struct Contour {
    path: BezierPath,
    points: Vec<Coord>,
    signed_area: f64,
    parent: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Outer,
    Hole,
    Redundant,
}

fn classify_parents(contours: &mut [Contour]) {
    for child_index in 0..contours.len() {
        let Some(sample) = contours[child_index].points.first().copied() else {
            continue;
        };
        let child_area = contours[child_index].signed_area.abs();
        contours[child_index].parent = contours
            .iter()
            .enumerate()
            .filter(|(candidate_index, candidate)| {
                *candidate_index != child_index
                    && candidate.signed_area.abs() > child_area
                    && point_in_polygon(sample, &candidate.points)
            })
            .min_by(|(_, left), (_, right)| {
                left.signed_area.abs().total_cmp(&right.signed_area.abs())
            })
            .map(|(index, _)| index);
    }
}

fn boundary_kind(
    index: usize,
    contour: &Contour,
    contours: &[Contour],
    fill_rule: FillRule,
) -> Boundary {
    let mut depth = 0_i32;
    let mut outside_winding = 0_i32;
    let mut parent = contour.parent;
    while let Some(parent_index) = parent {
        depth += 1;
        outside_winding += winding_sign(contours[parent_index].signed_area);
        parent = contours[parent_index].parent;
    }

    let (outside_filled, inside_filled) = match fill_rule {
        FillRule::EvenOdd => (depth % 2 != 0, (depth + 1) % 2 != 0),
        FillRule::NonZero => (
            outside_winding != 0,
            outside_winding + winding_sign(contours[index].signed_area) != 0,
        ),
    };

    match (outside_filled, inside_filled) {
        (false, true) => Boundary::Outer,
        (true, false) => Boundary::Hole,
        _ => Boundary::Redundant,
    }
}

fn closest_outer_ancestor(
    index: usize,
    contours: &[Contour],
    boundaries: &[Boundary],
) -> Option<usize> {
    let mut parent = contours[index].parent;
    while let Some(parent_index) = parent {
        if boundaries[parent_index] == Boundary::Outer {
            return Some(parent_index);
        }
        parent = contours[parent_index].parent;
    }
    None
}

fn winding_sign(area: f64) -> i32 {
    if area.is_sign_positive() { 1 } else { -1 }
}

fn signed_area(points: &[Coord]) -> f64 {
    points
        .windows(2)
        .map(|pair| pair[0].x * pair[1].y - pair[1].x * pair[0].y)
        .sum::<f64>()
        / 2.0
}

fn point_in_polygon(point: Coord, polygon: &[Coord]) -> bool {
    let mut inside = false;
    for edge in polygon.windows(2) {
        let first = edge[0];
        let second = edge[1];
        let crosses_y = (first.y > point.y) != (second.y > point.y);
        if crosses_y {
            let crossing_x =
                (second.x - first.x) * (point.y - first.y) / (second.y - first.y) + first.x;
            if point.x < crossing_x {
                inside = !inside;
            }
        }
    }
    inside
}

fn split_contours(
    path: &usvg::tiny_skia_path::Path,
    transform: usvg::Transform,
    coordinates: CoordinateSystem,
) -> Result<Vec<BezierPath>> {
    let mut contours = Vec::new();
    let mut segments = Vec::new();
    let mut start = None;
    let mut current = None;

    for command in path.segments() {
        use usvg::tiny_skia_path::PathSegment;
        match command {
            PathSegment::MoveTo(point) => {
                finish_contour(&mut contours, &mut segments, start, current)?;
                let point = convert_point(point, transform, coordinates);
                start = Some(point);
                current = Some(point);
            }
            PathSegment::LineTo(point) => {
                let end = convert_point(point, transform, coordinates);
                append_segment(&mut segments, &mut start, &mut current, None, end)?;
            }
            PathSegment::QuadTo(handle, point) => {
                let from = current.ok_or_else(|| {
                    ConversionError::new("SVG quadratic curve appears before its move command")
                })?;
                let handle = convert_point(handle, transform, coordinates);
                let end = convert_point(point, transform, coordinates);
                let handle1 = from + (handle - from) * (2.0 / 3.0);
                let handle2 = end + (handle - end) * (2.0 / 3.0);
                append_segment(
                    &mut segments,
                    &mut start,
                    &mut current,
                    Some((handle1, handle2)),
                    end,
                )?;
            }
            PathSegment::CubicTo(handle1, handle2, point) => {
                let handles = (
                    convert_point(handle1, transform, coordinates),
                    convert_point(handle2, transform, coordinates),
                );
                let end = convert_point(point, transform, coordinates);
                append_segment(&mut segments, &mut start, &mut current, Some(handles), end)?;
            }
            PathSegment::Close => {
                finish_contour(&mut contours, &mut segments, start, current)?;
                current = start;
                start = None;
            }
        }
    }
    finish_contour(&mut contours, &mut segments, start, current)?;
    Ok(contours)
}

fn append_segment(
    segments: &mut Vec<BezierSegment>,
    start: &mut Option<Coord>,
    current: &mut Option<Coord>,
    handles: Option<(Coord, Coord)>,
    end: Coord,
) -> Result<()> {
    let from = current
        .ok_or_else(|| ConversionError::new("SVG path segment appears before its move command"))?;
    if start.is_none() {
        *start = Some(from);
    }
    segments.push(BezierSegment::new(from, handles, end));
    *current = Some(end);
    Ok(())
}

fn finish_contour(
    contours: &mut Vec<BezierPath>,
    segments: &mut Vec<BezierSegment>,
    start: Option<Coord>,
    current: Option<Coord>,
) -> Result<()> {
    if segments.is_empty() {
        return Ok(());
    }
    let start = start.ok_or_else(|| ConversionError::new("SVG contour has no start point"))?;
    let current = current.ok_or_else(|| ConversionError::new("SVG contour has no end point"))?;
    if current != start {
        segments.push(BezierSegment::new(current, None, start));
    }
    let segment_count = segments.len();
    let path = BezierPath::new(
        BezierString::new(std::mem::take(segments)),
        vec![false; segment_count + 1],
    )
    .map_err(|error| ConversionError::new(format!("invalid converted SVG contour: {error}")))?;
    contours.push(path);
    Ok(())
}

fn convert_point(
    mut point: usvg::tiny_skia_path::Point,
    transform: usvg::Transform,
    coordinates: CoordinateSystem,
) -> Coord {
    transform.map_point(&mut point);
    Coord {
        x: (f64::from(point.x) - coordinates.center_x_px) * coordinates.mm_per_px,
        y: (coordinates.center_y_px - f64::from(point.y)) * coordinates.mm_per_px,
    }
}
