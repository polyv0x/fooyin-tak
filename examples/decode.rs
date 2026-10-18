// SPDX-License-Identifier: GPL-3.0-or-later
//! Decode to full-scale signed 32-bit little-endian PCM on stdout.
use fooyin_tak::{Decoder, DecoderOptions, RecoveryMode, Verification};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: decode [--verify-md5|--no-verify-md5] [--recovery strict|silence|skip]
              [--status] [--] INPUT.tak|- [SEEK_MILLISECONDS] > output.s32le
  -                  read sequential TAK input from stdin (no seeking)
  --verify-md5       verify stored PCM MD5 when present (default)
  --no-verify-md5    disable whole-PCM MD5 verification; frame CRCs remain checked
  --recovery MODE    strict (default), silence damaged ranges, or skip them
  --status           report encoder, preserved WAV data, MD5 status and damage
Seeking or recovery makes whole-PCM verification incomplete.";

struct Args {
    path: OsString,
    seek_ms: Option<u64>,
    options: DecoderOptions,
    status: bool,
}

fn parse_args() -> Result<Option<Args>, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut positional = Vec::new();
    let mut options = DecoderOptions::default();
    let mut status = false;
    let mut literal = false;
    while let Some(arg) = args.next() {
        if !literal {
            match arg.to_str() {
                Some("--") => {
                    literal = true;
                    continue;
                }
                Some("--help" | "-h") => return Ok(None),
                Some("--status") => {
                    status = true;
                    continue;
                }
                Some("--verify-md5") => {
                    options.verify_md5 = true;
                    continue;
                }
                Some("--no-verify-md5") => {
                    options.verify_md5 = false;
                    continue;
                }
                Some("--recovery") => {
                    options.recovery = match args.next().as_deref().and_then(|arg| arg.to_str()) {
                        Some("strict") => RecoveryMode::Strict,
                        Some("silence") => RecoveryMode::Silence,
                        Some("skip") => RecoveryMode::Skip,
                        _ => return Err("--recovery requires strict, silence, or skip".into()),
                    };
                    continue;
                }
                Some(value) if value.starts_with('-') && value != "-" => {
                    return Err(format!("unknown option: {value}").into());
                }
                _ => {}
            }
        }
        positional.push(arg);
    }
    if positional.is_empty() || positional.len() > 2 {
        return Err(USAGE.into());
    }
    let seek_ms = positional
        .get(1)
        .map(|arg| {
            arg.to_str()
                .ok_or("invalid seek milliseconds")?
                .parse::<u64>()
                .map_err(|_| "invalid seek milliseconds")
        })
        .transpose()?;
    let path = positional.remove(0);
    if path == "-" && seek_ms.is_some() {
        return Err("cannot seek stdin; omit SEEK_MILLISECONDS for sequential input".into());
    }
    Ok(Some(Args {
        path,
        seek_ms,
        options,
        status,
    }))
}

fn verification_text(value: Verification) -> &'static str {
    match value {
        Verification::NotPresent => "not present",
        Verification::Disabled => "disabled",
        Verification::Pending => "pending",
        Verification::Verified => "verified",
        Verification::Mismatch => "mismatch",
        Verification::Incomplete => "incomplete",
    }
}

fn decode<R: Read>(
    mut decoder: Decoder<R>,
    args: &Args,
    output: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(milliseconds) = args.seek_ms {
        decoder.seek_ms(milliseconds)?;
    }
    let info = decoder.info();
    let length = if decoder.length_known() {
        format!("{} sample frames", info.total_samples)
    } else {
        "unknown length".to_owned()
    };
    eprintln!(
        "{} Hz, {} channels, {} bits, {length}",
        info.sample_rate, info.channels, info.bits_per_sample
    );
    if args.status {
        if let Some(encoder) = &decoder.metadata().encoder {
            let evaluation = match encoder.evaluation {
                Some(0) => "standard",
                Some(1) => "extra",
                Some(2) => "maximum",
                _ => "unknown",
            };
            eprintln!(
                "encoder: {}.{}.{}, preset {}, evaluation {evaluation}, codec {}",
                encoder.version >> 16,
                (encoder.version >> 8) & 255,
                encoder.version & 255,
                encoder.preset,
                decoder.codec()
            );
        }
        if let Some(wave) = &decoder.metadata().wave {
            eprintln!(
                "preserved WAV: {} header bytes, {} trailer bytes",
                wave.head.len(),
                wave.tail.len()
            );
        }
    }
    let result = (|| {
        let mut samples = vec![0; 4096 * usize::from(info.channels)];
        loop {
            let count = decoder.read_samples(&mut samples)?;
            if count == 0 {
                break;
            }
            for sample in &samples[..count] {
                output.write_all(
                    &sample
                        .wrapping_shl(u32::from(32 - info.bits_per_sample))
                        .to_le_bytes(),
                )?;
            }
        }
        output.flush()?;
        Ok(())
    })();
    if args.status || !decoder.damage().is_empty() {
        eprintln!("MD5: {}", verification_text(decoder.verification()));
        eprintln!(
            "position: {} sample frames; total: {}",
            decoder.position(),
            decoder.info().total_samples
        );
        for damage in decoder.damage() {
            eprintln!(
                "damaged sample frames: {}..{}",
                damage.start,
                damage.start + damage.samples
            );
        }
    }
    result
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(args) = parse_args()? else {
        eprintln!("{USAGE}");
        return Ok(());
    };
    let mut output = BufWriter::new(io::stdout().lock());
    if args.path == "-" {
        let decoder = Decoder::stream_with_options(io::stdin().lock(), args.options)?;
        decode(decoder, &args, &mut output)
    } else {
        let decoder = Decoder::with_options(BufReader::new(File::open(&args.path)?), args.options)?;
        decode(decoder, &args, &mut output)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("decode: {error}");
            ExitCode::FAILURE
        }
    }
}
