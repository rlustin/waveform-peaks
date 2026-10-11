//! Generates a `.dat` or `.json` waveform from an Ogg Opus or WebM Opus file.
//!
//! Usage: generate <input> <output.dat|output.json> [zoom] [8|16] [split]

use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader},
    path::Path,
    process::ExitCode,
};

use tempfile::NamedTempFile;
use waveform_peaks::{
    Bits, ChannelMode, DatWriter, Error, JsonWriter, Limits, OggOpusSource, Options, PcmSource,
    WebmOpusSource, generate,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: {} <input> <output.dat|output.json> [zoom] [8|16] [split]",
            args[0]
        );
        return ExitCode::FAILURE;
    }
    let mut options = Options::default();
    if let Some(zoom) = args.get(3) {
        options.samples_per_pixel = zoom.parse().expect("zoom is a number");
    }
    if args.get(4).is_some_and(|bits| bits == "8") {
        options.bits = Bits::Eight;
    }
    if args.get(5).is_some_and(|mode| mode == "split") {
        options.channel_mode = ChannelMode::Split;
    }
    match run(&args[1], &args[2], &options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// Picks the container from the first bytes rather than the file name.
fn open(path: &str) -> Result<Box<dyn PcmSource>, Error> {
    let mut input = BufReader::new(File::open(path)?);
    let magic = input.fill_buf()?;
    if magic.starts_with(b"OggS") {
        Ok(Box::new(OggOpusSource::new(input)?))
    } else if magic.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Ok(Box::new(WebmOpusSource::new(input)?))
    } else {
        Err(Error::Unsupported("neither Ogg nor WebM".into()))
    }
}

fn run(input: &str, output: &str, options: &Options) -> Result<(), Error> {
    let source = &mut *open(input)?;
    // A failed run leaves a partial waveform, so only a complete one replaces
    // `output`. The temporary file is deleted when dropped on error.
    let dir = Path::new(output).parent().unwrap_or(Path::new("."));
    let mut file = NamedTempFile::new_in(dir)?;
    if output.ends_with(".json") {
        generate(
            source,
            options,
            &Limits::default(),
            JsonWriter::new(&mut file),
        )?;
    } else {
        generate(
            source,
            options,
            &Limits::default(),
            DatWriter::new(&mut file),
        )?;
    }
    file.persist(output).map_err(io::Error::from)?;
    Ok(())
}
