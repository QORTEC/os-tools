// SPDX-FileCopyrightText: Copyright © 2020-2023 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;

use thiserror::Error;

use crate::Installation;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Root is invalid")]
    RootInvalid,
}

/// A Client is a connection to the underlying package management systems
pub struct Client {
    /// Root that we operate on
    installation: Installation,
}

impl Client {
    /// Construct a new Client
    pub fn new_for_root(root: impl Into<PathBuf>) -> Result<Client, Error> {
        let root = root.into();

        if !root.exists() || !root.is_dir() {
            Err(Error::RootInvalid)
        } else {
            Ok(Client {
                installation: Installation::open(root),
            })
        }
    }

    /// Construct a new Client for the global installation
    pub fn system() -> Result<Client, Error> {
        Client::new_for_root("/")
    }
}

impl Drop for Client {
    // Automatically drop resources for the client
    fn drop(&mut self) {}
}
