use super::*;
use anyhow::{anyhow, Result};
use log::{debug, trace};
use std::convert::TryInto;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Read, Write};

use std::path::PathBuf;

pub mod bandaid;
pub mod interactive;

pub(crate) use bandaid::*;
use interactive::*;

/// correct all lines
/// `bandaids` are the fixes to be applied to the lines
///
/// Note that `Lines` as created by `(x as BufLines).lines()` does
/// not preserve trailing newlines, so either the iterator
/// needs to be modified to yield an extra (i.e. with `.chain("".to_owned())`)
/// or a manual newlines has to be written to the `sink`.
fn correct_lines<'s>(
    mut bandaids: impl Iterator<Item = BandAid>,
    source: impl Iterator<Item = (usize, String)>,
    mut sink: impl Write,
) -> Result<()> {
    let mut nxt: Option<BandAid> = bandaids.next();
    for (line_number, content) in source {
        trace!("Processing line {}", line_number);
        let mut remainder_column = 0usize;
        // let content: String = content.map_err(|e| {
        //     anyhow!("Line {} contains invalid utf8 characters", line_number).context(e)
        // })?;

        if nxt.is_none() {
            // no candidates remaining, just keep going
            sink.write(content.as_bytes())?;
            sink.write("\n".as_bytes())?;
            continue;
        }

        if let Some(ref bandaid) = nxt {
            if !bandaid.span.covers_line(line_number) {
                sink.write(content.as_bytes())?;
                sink.write("\n".as_bytes())?;
                continue;
            }
        }

        while let Some(bandaid) = nxt.take() {
            trace!("Applying next bandaid {:?}", bandaid);
            trace!("where line {} is: >{}<", line_number, content);
            let range: Range = bandaid
                .span
                .try_into()
                .expect("There should be no multiline strings as of today");
            // write prelude for this line between start or previous replacement
            if range.start > remainder_column {
                sink.write(content[remainder_column..range.start].as_bytes())?;
            }
            // write the replacement chunk
            sink.write(bandaid.replacement.as_bytes())?;

            remainder_column = range.end;
            nxt = bandaids.next();
            let complete_current_line = if let Some(ref bandaid) = nxt {
                // if `nxt` is also targeting the current line, don't complete the line
                !bandaid.span.covers_line(line_number)
            } else {
                true
            };
            if complete_current_line {
                // the last replacement may be the end of content
                if remainder_column < content.len() {
                    debug!(
                        "line {} len is {}, and remainder column is {}",
                        line_number,
                        content.len(),
                        remainder_column
                    );
                    // otherwise write all
                    // not that this also covers writing a line without any suggestions
                    sink.write(content[remainder_column..].as_bytes())?;
                } else {
                    debug!(
                        "line {} len is {}, and remainder column is {}",
                        line_number,
                        content.len(),
                        remainder_column
                    );
                }
                sink.write("\n".as_bytes())?;
                // break the inner loop
                break;
                // } else {
                // next suggestion covers same line
            }
        }
    }
    Ok(())
}

/// Mode in which `cargo-spellcheck` operates
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Action {
    /// Fix issues without interaction if there is sufficient information
    Fix,
    /// Only show errors
    Check,
    /// Interactively choose from __candidates__ provided, similar to `git add -p` .
    Interactive,
}

impl Action {
    /// assumes suggestions are sorted by line number and column number and must be non overlapping
    fn correction<'s>(
        &self,
        path: PathBuf,
        bandaids: impl IntoIterator<Item = BandAid>,
    ) -> Result<()> {
        let path = path
            .as_path()
            .canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize {}", path.display()).context(e))?;
        let path = dbg!(path.as_path());
        trace!("Attempting to open {} as read", path.display());
        let ro = std::fs::OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| anyhow!("Failed to open {}", path.display()).context(e))?;

        let mut reader = std::io::BufReader::new(ro);

        const TEMPORARY: &'static str = ".spellcheck.tmp";

        let tmp = std::env::current_dir()
            .expect("Must have cwd")
            .join(TEMPORARY);
        // let tmp = tmp.canonicalize().map_err(|e| { anyhow!("Failed to canonicalize {}", tmp.display() ).context(e) })?;
        //trace!("Attempting to open {} as read", tmp.display());
        let wr = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&tmp)
            .map_err(|e| anyhow!("Failed to open {}", path.display()).context(e))?;

        let mut writer = std::io::BufWriter::with_capacity(1024, wr);

        correct_lines(
            bandaids.into_iter(),
            (&mut reader)
                .lines()
                .filter_map(|line| line.ok())
                .enumerate()
                .map(|(lineno, content)| (lineno + 1, content)),
            &mut writer,
        )?;

        writer.flush()?;

        fs::rename(tmp, path)?;

        Ok(())
    }

    // consume self, doing the same thing again would cause garbage file content.
    pub fn write_changes_to_disk(&self, userpicked: UserPicked, _config: &Config) -> Result<()> {
        if userpicked.count() > 0 {
            debug!("Writing changes back to disk");
            for (path, bandaids) in userpicked.bandaids.into_iter() {
                self.correction(path, bandaids.into_iter())?;
            }
        } else {
            debug!("No band aids to apply");
        }
        Ok(())
    }

    /// Purpose was to check, check complete, so print the results.
    fn check(&self, suggestions_per_path: SuggestionSet, _config: &Config) -> Result<()> {
        let mut count = 0usize;
        for (_path, suggestions) in suggestions_per_path {
            count += suggestions.len();
            for suggestion in suggestions {
                eprintln!("{}", suggestion);
            }
        }
        if count > 0 {
            Err(anyhow::anyhow!(
                "Found {} potential spelling mistakes",
                count
            ))
        } else {
            Ok(())
        }
    }

    /// Run the requested action.
    pub fn run(self, suggestions_per_path: SuggestionSet, config: &Config) -> Result<()> {
        match self {
            Self::Fix => unimplemented!("Unsupervised fixing is not implemented just yet"),
            Self::Check => self.check(suggestions_per_path, config)?,
            Self::Interactive => {
                let picked =
                    interactive::UserPicked::select_interactive(suggestions_per_path, config)?;
                self.write_changes_to_disk(picked, config)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &'static str = r#"
I like unicorns every second Mondays.

"#;

    const CORRECTED: &'static str = r#"
I like banana icecream every third day.

"#;

    #[test]
    fn replace_unicorns() {
        let _ = env_logger::Builder::new()
            .filter(None, log::LevelFilter::Trace)
            .is_test(true)
            .try_init();

        let mut sink: Vec<u8> = Vec::with_capacity(1024);
        let bandaids = vec![
            BandAid {
                span: (2usize, 7..15).try_into().unwrap(),
                replacement: "banana icecream".to_owned(),
            },
            BandAid {
                span: (2usize, 22..28).try_into().unwrap(),
                replacement: "third".to_owned(),
            },
            BandAid {
                span: (2usize, 29..36).try_into().unwrap(),
                replacement: "day".to_owned(),
            },
        ];

        let lines = TEXT
            .lines()
            .map(|line| line.to_owned())
            .enumerate()
            .map(|(lineno, content)| (lineno + 1, content));

        correct_lines(bandaids.into_iter(), lines, &mut sink).expect("should be able to");

        assert_eq!(String::from_utf8_lossy(sink.as_slice()), CORRECTED);
    }
}
