use base64::prelude::{Engine, BASE64_STANDARD};
use clap::Parser;
use image::imageops::FilterType;
use image::io::Reader as ImageReader;
use image::{DynamicImage, Rgb};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path;

/// Replace preview image in the G-code with a one that is suitable for for MKS TFT35 display
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the G-code file.
    path: path::PathBuf,

    /// The size of the simage
    #[arg(short, long, default_value_t = 50)]
    simage_size: u8,

    /// The size of the gimage
    #[arg(short, long, default_value_t = 200)]
    gimage_size: u16,

    /// Log file
    #[arg(long)]
    log_file: Option<path::PathBuf>,

    ///Log level. Possible levels are OFF, DEBUG, INFO, WARN, ERROR
    #[arg(long, default_value_t = log::LevelFilter::Warn)]
    log_level: log::LevelFilter,
}

fn main() {
    let args = Args::parse();

    let _ = init_logging(&args.log_file, args.log_level);
    match do_main(&args) {
        Ok(_) => log::debug!("Finished successfully"),
        Err(_) => log::debug!("Finished with errors. Do not fail, to let the slicer continue"),
    }
}

fn do_main(args: &Args) -> Result<(), ()> {
    let (gcode_lines, image_lines) = read_gcode(&args.path)?;

    if image_lines.is_empty() {
        log::warn!("There is no image in gcode file. Leaving the original file unchanged");
        return Ok(());
    }

    log::debug!("Decoding base64 image from gcode");
    // `image` reader is good in guessing the image format, so we can just skip
    // `thumbnail_* begin <width>x<height> <size>` and `thumbnail_* end` lines
    // here and process everything that is in between.
    let decoded = BASE64_STANDARD
        .decode(image_lines[1..image_lines.len() - 1].join(""))
        .map_err(|e| log::error!("Cannot base64 decode image from gcode: {}", e))?;

    log::debug!("Guessing image format");
    let img = ImageReader::new(Cursor::new(decoded))
        .with_guessed_format()
        .expect("We are running on in-memory data for image. This should not fail");

    let img_format = match img.format().map(|format| format.extensions_str()) {
        Some([ext, ..]) => ext,
        _ => "UNKNOWN",
    };

    log::debug!("Decoding image as {}", img_format);
    let img = img.decode().map_err(|e| {
        log::error!("Cannot decode image. Guessed format: {}. Error: {}", img_format, e)
    })?;
    log::debug!("{}x{} {} image has been decoded", img.width(), img.height(), img_format);

    let simage = create_tft_image_gcode(
        ";simage",
        img.resize(args.simage_size.into(), args.simage_size.into(), FilterType::CatmullRom),
    );
    let gimage = create_tft_image_gcode(
        ";;gimage",
        img.resize(args.gimage_size.into(), args.gimage_size.into(), FilterType::CatmullRom),
    );

    // There is a possibility that we can corrupt the gcode file here if writing
    // fails mid process. I guess we could write to a temporary file first and
    // then, overwrite the original file. But I'll take the risk of leaving it
    // as it is for now.
    log::debug!("Writing gcode with converted image back to {}", args.path.display());
    let mut file = File::create(&args.path)
        .map_err(|e| log::error!("Failed to open original gcode file for writing: {}", e))?;

    file.write_all(simage.as_bytes()).map_err(|e| log::error!("Failed to write simage: {}", e))?;
    file.write_all(gimage.as_bytes()).map_err(|e| log::error!("Failed to write gimage: {}", e))?;
    file.write_all(gcode_lines[..gcode_lines.len() - 1].join("\n").as_bytes())
        .map_err(|e| log::error!("Failed to write original gcode header: {}", e))?;
    file.write_all(
        format!(
            "\n; MKS_TFT_PREVIEW_POSTPROCESS\n\
            ; Post processed by mks_tft_img v{} ({})\n\
            ;  The original {} image was removed from here. Its size was {}x{}\n\
            ;  simage = {}\n\
            ;  gimage = {}\n",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY"),
            img_format,
            img.width(),
            img.height(),
            args.simage_size,
            args.gimage_size
        )
        .as_bytes(),
    )
    .map_err(|e| log::error!("Failed to write postprocessing info: {}", e))?;
    file.write_all(gcode_lines[gcode_lines.len() - 1].as_bytes())
        .map_err(|e| log::error!("Failed to write original gcode: {}", e))?;

    Ok(())
}

/// Convert an RGB pixel to the RGB565 format
///
/// # Arguments
///
/// * `pixel` - A reference to an Rgb pixel
///
/// # Returns
///
/// A tuple containing the higher and lower bytes of the RGB565 color
fn rgb565(pixel: &Rgb<u8>) -> (u8, u8) {
    let r = (pixel.0[0] as u16) >> 3;
    let g = (pixel.0[1] as u16) >> 2;
    let b = (pixel.0[2] as u16) >> 3;
    let color = r << 11 | g << 5 | b;
    ((color >> 8) as u8, (color & 0xFF) as u8)
}

/// Create G-code representation of a TFT image
///
/// # Arguments
///
/// * `prefix` - A string prefix for the G-code
/// * `image` - The image to be converted
///
/// # Returns
///
/// A string containing the G-code for the image
fn create_tft_image_gcode(prefix: &str, image: DynamicImage) -> String {
    log::debug!(
        "Creating tft image gcode with prefix `{}` and size {}x{}",
        prefix,
        image.width(),
        image.height()
    );
    let mut tft_image = Vec::with_capacity(image.height() as usize);
    let mut tft_line = Vec::with_capacity(image.width() as usize);

    for (i, pixel) in image.to_rgb8().pixels().enumerate() {
        let (higher, lower) = rgb565(pixel);
        tft_line.push(format!("{:02x}{:02x}", lower, higher));

        if (i + 1) % image.width() as usize == 0 {
            tft_image.push(tft_line.join(""));
            tft_line.clear();
        }
    }

    format!("{}:{}\nM10086 ;\n", prefix, tft_image.join("\rM10086 ;"))
}

/// Read G-code from a file and extract image data
///
/// The image data is expected between `THUMBNAIL_BLOCK_START` and
/// `THUMBNAIL_BLOCK_END` comments. Every string that is found before this
/// block is added to the G-code lines vector unchanged, line by line (usually
/// this is a header comment generated by the slicer). The comments are not
/// added. Content between them is added to image lines vector. Each line is
/// trimmed and the `;` symbol in the beginning is also removed. The rest of
/// the G-code is added as a single unchanged string as the last element of the
/// G-code lines vector.  
///
/// # Arguments
///
/// * `path` - Path to the gcode file
///
/// # Returns
///
/// A tuple containing a vector of G-code lines and a vector of image lines
fn read_gcode(path: &path::PathBuf) -> Result<(Vec<String>, Vec<String>), ()> {
    log::info!("Reading gcode from `{}`", path.display());
    let mut reader =
        BufReader::new(File::open(path).map_err(|e| {
            log::error!("Cannot open file `{}` for reading: {}", path.display(), e)
        })?);

    let mut gcode_lines = vec![];
    let mut image_lines = vec![];
    let mut reading_image = false;

    for line_result in reader.by_ref().lines() {
        let line = line_result.map_err(|e| log::error!("Failed to read from gcode file: {}", e))?;
        if line.contains("THUMBNAIL_BLOCK_START") {
            log::debug!("THUMBNAIL_BLOCK_START found");
            reading_image = true;
            continue;
        }
        if line.contains("THUMBNAIL_BLOCK_END") {
            log::debug!("THUMBNAIL_BLOCK_END found");
            break;
        }
        if reading_image {
            let clean_line = line.trim_start_matches(';').trim();
            if !clean_line.is_empty() {
                image_lines.push(clean_line.to_string());
            }
        } else {
            gcode_lines.push(line);
        }
    }
    let mut reminder = String::new();
    reader
        .read_to_string(&mut reminder)
        .map_err(|e| log::error!("Failed to read from gcode file: {}", e))?;
    gcode_lines.push(reminder);
    Ok((gcode_lines, image_lines))
}

/// Initialize logging
fn init_logging(log_file: &Option<path::PathBuf>, level: log::LevelFilter) -> Result<(), ()> {
    use simplelog::*;
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![];
    loggers.push(TermLogger::new(
        level,
        Config::default(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    ));
    if let Some(path) = log_file {
        loggers.push(WriteLogger::new(
            level,
            Config::default(),
            File::create(path).map_err(|e| {
                eprintln!("Failed to open log file {} for writing: {}", path.display(), e)
            })?,
        ))
    }
    CombinedLogger::init(loggers).expect("We don't expect any other loggers to be set");
    log::debug!("Logging initialized");
    Ok(())
}
