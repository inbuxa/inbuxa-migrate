/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Passwords for the accounts `import-directory` creates, and the private
//! file they are handed to the administrator in.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::error::Error;

/// Letters and digits with the look-alikes (`0 O o 1 l I`) left out, so a
/// password can be read aloud or typed from paper. No character a
/// spreadsheet would take for a formula.
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";

/// 24 characters from 56: about 139 bits, far past any strength check.
const LENGTH: usize = 24;

pub fn generate() -> Result<String, Error> {
    let mut out = String::with_capacity(LENGTH);
    // The largest multiple of the alphabet's size below 256, so every
    // character is equally likely.
    let limit = 256 - 256 % ALPHABET.len();
    let mut buf = [0u8; 64];
    while out.len() < LENGTH {
        getrandom::fill(&mut buf)
            .map_err(|e| Error::Partial(format!("no random numbers for a password: {e}")))?;
        for &b in &buf {
            if (b as usize) < limit && out.len() < LENGTH {
                out.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            }
        }
    }
    Ok(out)
}

/// `email,password` rows, one per account created, in a file that is new
/// and readable by its owner only. Rows are written as each batch of
/// accounts is made, so a run that stops part way keeps the passwords of
/// the accounts it made.
pub struct PasswordFile {
    file: File,
}

impl PasswordFile {
    pub fn create(path: &Path) -> Result<Self, Error> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => Error::Usage(format!(
                "{} already exists; passwords go into a new file",
                path.display()
            )),
            _ => Error::Io(e),
        })?;
        file.write_all(b"email,password\n")?;
        Ok(PasswordFile { file })
    }

    pub fn add(&mut self, email: &str, password: &str) -> Result<(), Error> {
        // Addresses hold no comma or quote, and passwords are letters and
        // digits, so no field needs quoting.
        writeln!(self.file, "{email},{password}")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), Error> {
        self.file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_long_varied_and_from_the_alphabet() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_eq!(a.len(), LENGTH);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)), "{a}");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_new_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("passwords.csv");
        let mut f = PasswordFile::create(&path).unwrap();
        f.add("ann@example.org", "Secret23").unwrap();
        f.flush().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "email,password\nann@example.org,Secret23\n"
        );
        let err = PasswordFile::create(&path).err().unwrap().to_string();
        assert!(err.contains("already exists"), "{err}");
    }
}
