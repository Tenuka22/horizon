//! `horizon-launcher account sign-in`: the Ely.by sign-in in the terminal. The password is typed
//! without echo or read from stdin (`--password-stdin`), never taken from the command line.

use horizon_launcher::account::{self, Accounts};
use std::io::{IsTerminal, Write};

/// Signs in to Ely.by. `who` is the username, or a signed-in account's name or UUID.
pub(crate) fn sign_in(
    accounts: &Accounts,
    who: Option<String>,
    password_stdin: bool,
    code: Option<String>,
    as_json: bool,
) -> Result<(), String> {
    let from_terminal = std::io::stdin().is_terminal();
    let username = match who {
        // A signed-in account's UUID works for its name.
        Some(who) => accounts.find(&who).map_or(who, |a| a.name.clone()),
        None if from_terminal => prompt_line("Ely.by username or email: ")?,
        None => return Err("Add --username NAME (or give the name).".into()),
    };
    let mut code = code;
    let signed_in = loop {
        let password = if from_terminal {
            prompt_password(&format!("Password for {username}: "))?
        } else if password_stdin {
            read_password_stdin()?
        } else {
            return Err(
                "Not a terminal: add --password-stdin and give the password on stdin.".into(),
            );
        };
        match account::elyby_sign_in(&username, &password, code.as_deref()) {
            Err(e) if e == account::elyby::CODE_NEEDED && from_terminal => {
                eprintln!("{e}");
                code = Some(prompt_line("Two-factor code: ")?);
            }
            Err(e) if e == account::elyby::CODE_NEEDED => {
                return Err(format!("{e} Add --code CODE."));
            }
            result => break result?,
        }
    };
    if as_json {
        super::json(serde_json::json!({ "signed_in": signed_in.uuid, "name": signed_in.name }));
    } else {
        println!("Signed in as {}.", signed_in.name);
    }
    if let Some(said) = account::no_keychain() {
        eprintln!("{said}");
    }
    Ok(())
}

/// A line typed at the terminal, after `prompt` (on stderr, so `--json` output stays clean).
fn prompt_line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

/// A password typed at the terminal: nothing is echoed.
fn prompt_password(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let password = rpassword::read_password().map_err(|e| e.to_string())?;
    if password.is_empty() {
        return Err("No password given.".into());
    }
    Ok(password)
}

/// The first line of stdin, which is the password for `--password-stdin` (only its line ending
/// goes: a password may begin or end with spaces).
fn read_password_stdin() -> Result<String, String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let password = line.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Err("No password on stdin.".into());
    }
    Ok(password.to_string())
}
