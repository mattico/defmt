use std::{
    env, fs,
    io::{self, Read, Write},
    path::PathBuf,
};

use anyhow::anyhow;
use defmt_decoder::Table;
use serialport::{COMPort, SerialPort};
use structopt::StructOpt;

/// Prints defmt-encoded logs to stdout
#[derive(StructOpt)]
#[structopt(name = "defmt-print")]
struct Opts {
    #[structopt(short, parse(from_os_str), required_unless_one(&["version"]))]
    elf: Option<PathBuf>,

    #[structopt(short = "V", long)]
    version: bool,
    // may want to add this later
    // #[structopt(short, long)]
    // verbose: bool,
    // TODO add file path argument; always use stdin for now
    /// A serial port device to use for receiving defmt messages.
    #[structopt(long)]
    pub(crate) serial: Option<String>,

    /// The baud rate to use for the serial port.
    #[structopt(long, default_value = "115200")]
    pub(crate) baud_rate: u32,
}

const READ_BUFFER_SIZE: usize = 1024;

fn main() -> anyhow::Result<()> {
    let opts: Opts = Opts::from_args();

    if opts.version {
        return print_version();
    }

    let verbose = false;
    defmt_decoder::log::init_logger(verbose, |metadata| {
        // We display *all* defmt frames, but nothing else.
        defmt_decoder::log::is_defmt_frame(metadata)
    });

    let bytes = fs::read(&opts.elf.unwrap())?;

    let table = Table::parse(&bytes)?.ok_or_else(|| anyhow!(".defmt data not found"))?;
    let locs = table.get_locations(&bytes)?;

    let locs = if table.indices().all(|idx| locs.contains_key(&(idx as u64))) {
        Some(locs)
    } else {
        log::warn!("(BUG) location info is incomplete; it will be omitted from the output");
        None
    };

    let stdin = io::stdin();
    let mut reader: Box<dyn Read> = if let Some(port) = &opts.serial {
        Box::new(setup_serial_port(port, opts.baud_rate)?)
    } else {
        Box::new(stdin.lock())
    };

    let mut buf = [0; READ_BUFFER_SIZE];
    let mut frames = vec![];

    let current_dir = env::current_dir()?;

    loop {
        let n = reader.read(&mut buf)?;

        frames.extend_from_slice(&buf[..n]);

        let mut data = &frames[..];
        while let Some(end_idx) = data.iter().position(|x| *x == 0) {
            if end_idx > 0 {
                // TODO(defmt 0.3): rzCOBS will be part of the defmt decoding
                match rzcobs::decode(&data[..end_idx]) {
                    Ok(msg) => {
                        match table.decode(&msg) {
                            Ok((frame, _consumed)) => {
                                // NOTE(`[]` indexing) all indices in `table` have already been
                                // verified to exist in the `locs` map
                                let loc = locs.as_ref().map(|locs| &locs[&frame.index()]);

                                let (mut file, mut line, mut mod_path) = (None, None, None);
                                if let Some(loc) = loc {
                                    let relpath =
                                        if let Ok(relpath) = loc.file.strip_prefix(&current_dir) {
                                            relpath
                                        } else {
                                            // not relative; use full path
                                            &loc.file
                                        };
                                    file = Some(relpath.display().to_string());
                                    line = Some(loc.line as u32);
                                    mod_path = Some(loc.module.clone());
                                }

                                // Forward the defmt frame to our logger.
                                defmt_decoder::log::log_defmt(
                                    &frame,
                                    file.as_deref(),
                                    line,
                                    mod_path.as_deref(),
                                );
                            }
                            Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
                            Err(defmt_decoder::DecodeError::Malformed) => {
                                log::error!("failed to decode defmt data: {:x?}", frames);
                                return Err(defmt_decoder::DecodeError::Malformed.into());
                            }
                        }
                    }
                    Err(_) => {
                        log::error!("Malformed rzCOBS frame of len {}", end_idx)
                    }
                }
            }
            let next_start = end_idx + 1;
            if frames.len() > next_start {
                data = &data[next_start..];
            } else {
                data = &[];
                break;
            }
        }
        let leftover = data.len();
        if leftover == 0 {
            frames.clear();
        } else {
            frames.rotate_right(leftover);
            frames.truncate(leftover);
        }
    }
}

/// Report version from Cargo.toml _(e.g. "0.1.4")_ and supported `defmt`-versions.
///
/// Used by `--version` flag.
#[allow(clippy::unnecessary_wraps)]
fn print_version() -> anyhow::Result<()> {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    println!("supported defmt version: {}", defmt_decoder::DEFMT_VERSION);
    Ok(())
}

fn setup_serial_port(port: &str, baud_rate: u32) -> anyhow::Result<COMPort> {
    let mut serial = serialport::new(port, baud_rate).open_native()?;

    #[cfg(windows)]
    {
        // Increase rx buffer size
        use std::os::windows::io::AsRawHandle;
        let handle = serial.as_raw_handle();

        let result = unsafe { winapi::um::commapi::SetupComm(handle, 4096, 1024 * 128) };
        if result == 0 {
            let err_code = unsafe { winapi::um::errhandlingapi::GetLastError() as i32 };
            return Err(io::Error::from_raw_os_error(err_code).into());
        }
    }

    serial.clear(serialport::ClearBuffer::All)?;
    serial.write(&[b'c'])?; // Signal the target that we're ready for data

    Ok(serial)
}
