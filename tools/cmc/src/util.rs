// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::Error;
use cm_rust::ComponentDecl;
use fidl::unpersist;
use fidl_fuchsia_component_decl::Component;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Read a JSON or JSON5 file.
/// Attempts to parse as JSON first.
/// If this fails, attempts to parse as JSON5.
/// Parsing with serde_json5 is known to be much slower, so we try the faster
/// parser first.
pub(crate) fn json_or_json5_from_file(file: &PathBuf) -> Result<Value, Error> {
    let mut buffer = String::new();
    fs::File::open(&file)
        .map_err(|e| {
            Error::invalid_args(format!(
                "Could not open file at path \"{}\": {}",
                file.display(),
                e
            ))
        })?
        .read_to_string(&mut buffer)?;

    serde_json::from_str(&buffer).or_else(|_| {
        // If JSON parsing fails, try JSON5 parsing (which is slower)
        serde_json5::from_str(&buffer).map_err(|e| {
            Error::parse(
                format!("Couldn't read {} as JSON: {}", file.display(), e),
                e.try_into().ok(),
                Some(file.as_path()),
            )
        })
    })
}

/// Write a depfile.
/// Given an output and its includes, writes a depfile in Make format.
/// If there is no output, deletes the potentially stale depfile.
pub(crate) fn write_depfile(
    depfile: &PathBuf,
    output: Option<&PathBuf>,
    inputs: &Vec<PathBuf>,
) -> Result<(), Error> {
    if output.is_none() {
        // A non-existent depfile is the same as an empty depfile
        if depfile.exists() {
            // Delete stale depfile
            fs::remove_file(depfile)?;
        }
    } else if let Some(output_path) = output {
        #[allow(clippy::format_collect, reason = "mass allow for https://fxbug.dev/381896734")]
        let depfile_contents = format!(
            "{}:{}\n",
            output_path.display(),
            &inputs.iter().map(|i| format!(" {}", i.display())).collect::<String>()
        );
        fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(depfile)?
            .write_all(depfile_contents.as_bytes())?;
    }
    Ok(())
}

/// Read .cml file and parse into a cml::Document.
pub(crate) fn read_cml(file: &Path) -> Result<cml::Document, Error> {
    let buffer = read_file_to_string(file)?;
    cml::parse_one_document(&buffer, file)
}

/// Read .cml file and parse into one or more Documents. Some fuchsia.git GN rules
/// collect .cml files to merge using "GN metadata", which can only output its merged
/// representation as a JSON array. In the case of component GN rules, this is an
/// array of many .cml JSON objects.
pub(crate) fn read_cml_tolerate_gn_metadata(file: &Path) -> Result<Vec<cml::Document>, Error> {
    let buffer = read_file_to_string(file)?;
    cml::parse_many_documents(&buffer, file)
}

fn read_file_to_string(file: &Path) -> Result<String, Error> {
    let mut buffer = String::new();
    fs::File::open(&file)
        .map_err(|e| {
            Error::parse(format!("Couldn't read file {:?}: {}", file, e), None, Some(file))
        })?
        .read_to_string(&mut buffer)
        .map_err(|e| {
            Error::parse(format!("Couldn't read file {:?}: {}", file, e), None, Some(file))
        })?;
    Ok(buffer)
}

/// Read .cm file and parse into a cm_rust::ComponentDecl.
pub(crate) fn read_cm(file: &Path) -> Result<ComponentDecl, Error> {
    let mut buffer = vec![];
    fs::File::open(&file)
        .map_err(|e| Error::parse(format!("Couldn't open file: {}", e), None, Some(file)))?
        .read_to_end(&mut buffer)
        .map_err(|e| Error::parse(format!("Couldn't read file: {}", e), None, Some(file)))?;
    let fidl_component_decl: Component = unpersist(&buffer).map_err(|e| {
        Error::parse(format!("Couldn't decode bytes to Component FIDL: {}", e), None, Some(file))
    })?;
    ComponentDecl::try_from(fidl_component_decl).map_err(|e| {
        Error::parse(format!("Couldn't convert Component FIDL to cm_rust: {}", e), None, Some(file))
    })
}

pub(crate) fn ensure_directory_exists(output: &PathBuf) -> Result<(), Error> {
    if let Some(parent) = output.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn test_write_depfile() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = tmp_dir.path();
        let depfile = tmp_path.join("foo.d");
        let output = tmp_path.join("foo.cml");
        let includes = vec![tmp_path.join("bar.cml"), tmp_path.join("qux.cml")];
        write_depfile(&depfile, Some(&output), &includes).unwrap();

        let mut depfile_contents = String::new();
        File::open(&depfile).unwrap().read_to_string(&mut depfile_contents).unwrap();
        assert_eq!(
            depfile_contents,
            format!("{tmp}/foo.cml: {tmp}/bar.cml {tmp}/qux.cml\n", tmp = tmp_path.display())
        );
    }

    #[test]
    fn test_write_depfile_no_includes() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = tmp_dir.path();
        let depfile = tmp_path.join("foo.d");
        let output = tmp_path.join("foo.cml");
        let includes = vec![];
        write_depfile(&depfile, Some(&output), &includes).unwrap();

        let mut depfile_contents = String::new();
        File::open(&depfile).unwrap().read_to_string(&mut depfile_contents).unwrap();
        assert_eq!(depfile_contents, format!("{tmp}/foo.cml:\n", tmp = tmp_path.display()));
    }

    #[test]
    fn test_ensure_directory_exists() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = tmp_dir.path();
        let nested_directory = tmp_path.join("foo/bar");
        let nested_file = nested_directory.join("qux.cml");
        assert!(!nested_directory.exists());
        ensure_directory_exists(&nested_file).unwrap();
        assert!(nested_directory.exists());
        // Operation is idempotent
        ensure_directory_exists(&nested_file).unwrap();
        assert!(nested_directory.exists());
    }
}
