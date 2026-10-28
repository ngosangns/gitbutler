//! Git runs this when a fetch or push asks for a password.
//!
//! The wire protocol is the one `gitbutler-git` expects: connect to
//! `GITBUTLER_ASKPASS_PIPE`, write `GITBUTLER_ASKPASS_SECRET`, write the prompt
//! argument, then copy the reply to stdout. Do not print the secret or the reply.

use std::io::{BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gitbutler-git-askpass: {message}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), &'static str> {
    let pipe =
        std::env::var("GITBUTLER_ASKPASS_PIPE").map_err(|_| "missing GITBUTLER_ASKPASS_PIPE")?;
    let secret = std::env::var("GITBUTLER_ASKPASS_SECRET")
        .map_err(|_| "missing GITBUTLER_ASKPASS_SECRET")?;
    let prompt = std::env::args().nth(1).ok_or("missing prompt")?;
    speak(&pipe, &secret, &prompt)
}

#[cfg(unix)]
fn speak(pipe: &str, secret: &str, prompt: &str) -> Result<(), &'static str> {
    use std::os::unix::net::UnixStream;

    let mut writer = UnixStream::connect(pipe).map_err(|_| "could not reach gb-gpui")?;
    let mut reader_stream = writer.try_clone().map_err(|_| "could not read the reply")?;
    reader_stream
        .set_read_timeout(None)
        .map_err(|_| "could not wait for the reply")?;
    writeln!(writer, "{secret}").map_err(|_| "could not send the secret")?;
    writeln!(writer, "{prompt}").map_err(|_| "could not send the prompt")?;
    writer.flush().map_err(|_| "could not send the prompt")?;

    let mut password = String::new();
    let nread = std::io::BufReader::new(reader_stream)
        .read_line(&mut password)
        .map_err(|_| "could not read the reply")?;
    if nread == 0 {
        return Err("gb-gpui closed the prompt");
    }
    let mut stdout = std::io::stdout();
    stdout
        .write_all(password.as_bytes())
        .map_err(|_| "could not answer git")?;
    stdout.flush().map_err(|_| "could not answer git")?;
    Ok(())
}

#[cfg(not(unix))]
fn speak(_pipe: &str, _secret: &str, _prompt: &str) -> Result<(), &'static str> {
    Err("this helper only runs on macOS and Linux")
}
