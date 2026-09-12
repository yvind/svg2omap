use std::{
    fs,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
};

use clap::{Parser, ValueHint};
use omap::Code;
use svg_to_omap_point_symbol::{ConversionOptions, convert_svg};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Convert SVG artwork to one OMAP point symbol in an otherwise empty map",
    after_help = "If OUTPUT is omitted, INPUT's extension is replaced with .omap."
)]
struct Cli {
    /// SVG file to convert.
    #[arg(value_name = "INPUT.svg", value_hint = ValueHint::FilePath)]
    input: PathBuf,

    /// OMAP file to create.
    #[arg(value_name = "OUTPUT.omap", value_hint = ValueHint::FilePath)]
    output: Option<PathBuf>,

    /// Point-symbol name (defaults to the input file name).
    #[arg(long)]
    name: Option<String>,

    /// Point-symbol code.
    #[arg(long, default_value = "900", value_name = "A[.B[.C]]", value_parser = parse_code)]
    code: Code,

    /// Map scale denominator.
    #[arg(long, default_value = "10000", value_name = "N")]
    map_scale: NonZeroU32,

    /// SVG CSS pixel density.
    #[arg(long, default_value = "96", value_name = "DPI", value_parser = parse_positive_number)]
    dpi: f64,

    /// Override the physical SVG viewport width.
    #[arg(long, value_name = "MM", value_parser = parse_positive_number)]
    width_mm: Option<f64>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let data = fs::read(&cli.input)
        .map_err(|error| format!("could not read {}: {error}", cli.input.display()))?;
    let default_name = cli
        .input
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("SVG symbol");
    let mut options = ConversionOptions::new(cli.name.as_deref().unwrap_or(default_name));
    options.symbol_code = cli.code;
    options.map_scale = cli.map_scale;
    options.dpi = cli.dpi;
    options.width_mm = cli.width_mm;

    let output = cli
        .output
        .unwrap_or_else(|| default_output_path(&cli.input));
    let conversion =
        convert_svg(&data, cli.input.parent(), &options).map_err(|error| error.to_string())?;
    conversion
        .map
        .to_file(&output)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;

    println!(
        "Wrote {}: {:.3} x {:.3} mm, {} paint layers, {} point-symbol elements",
        output.display(),
        conversion.width_mm,
        conversion.height_mm,
        conversion.paint_layers,
        conversion.area_elements,
    );
    Ok(())
}

fn parse_code(raw: &str) -> Result<Code, String> {
    Code::from_str(raw).map_err(|_| format!("invalid symbol code {raw:?}; expected A[.B[.C]]"))
}

fn parse_positive_number(raw: &str) -> Result<f64, String> {
    let value = raw
        .parse::<f64>()
        .map_err(|_| format!("invalid positive number {raw:?}"))?;
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err("value must be a positive finite number".to_owned())
    }
}

fn default_output_path(input: &Path) -> PathBuf {
    input.with_extension("omap")
}
