//! The upload job: exported files → an album of this app's, with per-file
//! progress and per-file failures.

use std::path::PathBuf;

use crate::client::{Album, Client, NewItem};
use crate::{BATCH_LIMIT, GphotosError, MAX_DESCRIPTION_CHARS};

/// One exported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadFile {
    pub path: PathBuf,
    /// The photo's caption or title (trimmed, at most 1000 characters; empty = none).
    pub description: Option<String>,
}

/// Where the files go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumChoice {
    /// An album this app created earlier (picked from the list).
    Existing { id: String, title: String },
    /// This app's album with this title, created when there is none.
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Finding or creating the album.
    Album,
    /// Sending file `done + 1` of `total`.
    Uploading,
    /// Adding uploaded files to the library and album.
    Adding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    /// Files finished (added or failed).
    pub done: usize,
    pub total: usize,
}

/// What a job did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadReport {
    pub album: Option<Album>,
    /// File → new media item id.
    pub uploaded: Vec<(PathBuf, String)>,
    /// File → why it isn't in Google Photos.
    pub failed: Vec<(PathBuf, String)>,
    pub cancelled: bool,
}

fn description(d: Option<&str>) -> Option<String> {
    let d = d?.trim();
    (!d.is_empty()).then(|| d.chars().take(MAX_DESCRIPTION_CHARS).collect())
}

/// Upload `files` into `album`. Bytes go one file at a time; every
/// [`BATCH_LIMIT`] uploaded files are added in one `batchCreate` (calls run one
/// after another, as Google asks). A file that fails is reported and the rest go
/// on; a sign-in or rate-limit failure that would fail every later file stops the
/// job and fails what is left with that reason. Fails as a whole only when the
/// album can't be found or created.
pub fn upload(
    client: &mut Client<'_>,
    files: &[UploadFile],
    album: &AlbumChoice,
    progress: &mut dyn FnMut(Progress),
) -> Result<UploadReport, GphotosError> {
    let total = files.len();
    let mut report = UploadReport::default();
    progress(Progress {
        stage: Stage::Album,
        done: 0,
        total,
    });
    let album = match album {
        AlbumChoice::Existing { id, title } => Album {
            id: id.clone(),
            title: title.clone(),
            product_url: String::new(),
            media_items_count: None,
        },
        AlbumChoice::Named(title) => client.find_or_create_album(title)?,
    };
    let album_id = album.id.clone();
    report.album = Some(album);
    let mut pending: Vec<(PathBuf, NewItem)> = Vec::new();
    let mut stopped: Option<String> = None;
    let mut next = 0;
    while next < files.len() && stopped.is_none() {
        let f = &files[next];
        next += 1;
        progress(Progress {
            stage: Stage::Uploading,
            done: report.uploaded.len() + report.failed.len() + pending.len(),
            total,
        });
        match client.upload_file(&f.path) {
            Ok(token) => pending.push((
                f.path.clone(),
                NewItem {
                    upload_token: token,
                    file_name: f
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    description: description(f.description.as_deref()),
                },
            )),
            Err(GphotosError::Cancelled) => {
                report.cancelled = true;
                break;
            }
            Err(e) if e.is_fatal() => {
                report.failed.push((f.path.clone(), e.to_string()));
                stopped = Some(e.to_string());
            }
            Err(e) => report.failed.push((f.path.clone(), e.to_string())),
        }
        let full = pending.len() == BATCH_LIMIT || (next == files.len() && !pending.is_empty());
        if full
            && let Some(why) = add(
                client,
                &album_id,
                &mut pending,
                &mut report,
                total,
                progress,
            )
        {
            stopped = Some(why);
        }
    }
    if !pending.is_empty() && !report.cancelled {
        // Stopped mid-batch: add what was sent (it may still go through).
        add(
            client,
            &album_id,
            &mut pending,
            &mut report,
            total,
            progress,
        );
    }
    if report.cancelled {
        // Sent but never added: not in Google Photos.
        report.failed.extend(
            pending
                .drain(..)
                .map(|(p, _)| (p, "cancelled before it was added".to_owned())),
        );
    }
    if let Some(why) = stopped {
        report
            .failed
            .extend(files[next..].iter().map(|f| (f.path.clone(), why.clone())));
    }
    progress(Progress {
        stage: Stage::Adding,
        done: report.uploaded.len() + report.failed.len(),
        total,
    });
    Ok(report)
}

/// `batchCreate` the pending files. Returns the reason to stop the job, if any.
fn add(
    client: &mut Client<'_>,
    album_id: &str,
    pending: &mut Vec<(PathBuf, NewItem)>,
    report: &mut UploadReport,
    total: usize,
    progress: &mut dyn FnMut(Progress),
) -> Option<String> {
    progress(Progress {
        stage: Stage::Adding,
        done: report.uploaded.len() + report.failed.len(),
        total,
    });
    let batch: Vec<_> = std::mem::take(pending);
    let items: Vec<NewItem> = batch.iter().map(|(_, i)| i.clone()).collect();
    match client.batch_create(Some(album_id), &items) {
        Ok(results) => {
            for ((path, _), r) in batch.into_iter().zip(results) {
                match r {
                    Ok(id) => report.uploaded.push((path, id)),
                    Err(why) => report.failed.push((path, why)),
                }
            }
            None
        }
        Err(e) => {
            if matches!(e, GphotosError::Cancelled) {
                report.cancelled = true;
            }
            let why = e.to_string();
            report
                .failed
                .extend(batch.into_iter().map(|(p, _)| (p, why.clone())));
            e.is_fatal().then_some(why)
        }
    }
}

#[cfg(test)]
mod tests;
