//! `fs.readSessionImage`: the bytes of an image an agent reply shows, for anyone who may read the
//! conversation.
//!
//! Reading the conversation is the whole authority, so what the call reaches is held to what the
//! conversation already shows: a file that an agent reply of this session names as an image,
//! inside the session's own folder, whose name and header both say it is a raster image. A user
//! prompt or tool output names nothing; otherwise a `find` or `ls` the agent ran would make every
//! image it listed readable to every viewer.
//!
//! Neither the path nor the bytes are logged, and no refusal carries the path.

use super::*;
use std::path::{Path, PathBuf};

pub(crate) const METHOD: &str = "fs.readSessionImage";

#[derive(Debug, thiserror::Error)]
pub(crate) enum Refused {
    #[error("No agent reply in this conversation shows that image")]
    NotNamed,
    #[error("The image is outside the conversation's folder")]
    OutsideFolder,
    #[error("The file is not a PNG, JPEG, GIF or WebP image")]
    NotImage,
    #[error("The image is larger than the preview limit")]
    TooLarge,
    #[error("The image is missing")]
    Missing,
}

/// A refusal names its `kind` for the page to choose a fallback by; a failed history read keeps
/// the history reader's own kinds.
pub(crate) fn rpc_error(error: anyhow::Error) -> crate::protocol::RpcError {
    use crate::protocol::{ErrorCode, RpcError};
    let Some(refused) = error.downcast_ref::<Refused>() else {
        return super::rpc_error(error);
    };
    let kind = match refused {
        Refused::NotNamed => "not_named",
        Refused::OutsideFolder => "outside_folder",
        Refused::NotImage => "not_image",
        Refused::TooLarge => "too_large",
        Refused::Missing => "missing",
    };
    let mut result = RpcError::new(ErrorCode::PathNotAllowed, refused.to_string());
    result.data = Some(json!({"kind":kind,"retryable":false}));
    result
}

pub fn read(params: Value) -> crate::Result<Value> {
    let roster = super::super::roster::Roster::read_only()?;
    read_with_roster(&params, &roster)
}

fn read_with_roster(params: &Value, roster: &super::super::roster::Roster) -> crate::Result<Value> {
    let requested = params["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .context(Failure::Request)?;
    with_target(params, roster, |target| {
        let folder = Path::new(target.cwd);
        let path = resolve(folder, requested);
        let canonical = std::fs::canonicalize(&path).ok();
        // Whether a path no reply names exists, or where it leads, is not told: every such path
        // gets the same refusal.
        let named = snapshot::find_reply(target, |reply| {
            named_images(reply).iter().any(|name| {
                let name = resolve(folder, name);
                name == path
                    || canonical.is_some() && std::fs::canonicalize(&name).ok() == canonical
            })
        })?;
        ensure!(named, Refused::NotNamed);
        let canonical = canonical.ok_or(Refused::Missing)?;
        ensure!(
            std::fs::canonicalize(folder).is_ok_and(|folder| canonical.starts_with(folder)),
            Refused::OutsideFolder
        );
        read_image(&canonical)
    })
}

/// As the page resolves a path: an absolute one stays, a relative one is in the session's folder.
fn resolve(folder: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        folder.join(path)
    }
}

/// The paths a reply shows as images, found as the Workspace page finds them: the destination of
/// each CommonMark image under the GFM extensions the page renders (escapes and entities decoded,
/// nothing inside code or inside another image's alt text), and the `local_path` of each CodeLark
/// image tag outside code and image alt text, whose JSON holds exactly `msg_type` = `"image"` and
/// `local_path`. A tag quoted in a code span or block, or in alt text, is shown as text, so it
/// names nothing either.
fn named_images(reply: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    const OPEN: &str = "<clk-send>";
    const CLOSE: &str = "</clk-send>";
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut names = Vec::new();
    let mut as_text = Vec::new();
    // An image inside another image's alt text renders as that alt text, not as an image.
    let mut in_image = 0usize;
    for (event, range) in Parser::new_ext(reply, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if in_image == 0 {
                    names.push(dest_url.into_string());
                    // The alt text is shown as text, so a tag inside it names nothing.
                    as_text.push(range);
                }
                in_image += 1;
            }
            Event::End(TagEnd::Image) => in_image = in_image.saturating_sub(1),
            Event::Start(Tag::CodeBlock(_)) | Event::Code(_) => as_text.push(range),
            _ => {}
        }
    }
    let in_text = |at: usize| as_text.iter().any(|range| range.contains(&at));
    names.extend(reply.match_indices(OPEN).filter_map(|(start, _)| {
        if in_text(start) {
            return None;
        }
        let body = &reply[start + OPEN.len()..];
        let tag: serde_json::Map<String, Value> =
            serde_json::from_str(&body[..body.find(CLOSE)?]).ok()?;
        let path = tag.get("local_path")?.as_str()?;
        (tag.len() == 2 && tag.get("msg_type")? == "image").then(|| path.to_owned())
    }));
    names
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl Format {
    fn of_name(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "gif" => Self::Gif,
            "webp" => Self::Webp,
            _ => return None,
        })
    }

    fn of_header(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// The name and the header must agree, and the type answered is the header's: a script renamed
/// `.png` is not an image, and an SVG, which can carry script, is never one here.
fn read_image(path: &Path) -> crate::Result<Value> {
    use base64::Engine as _;
    use std::io::Read as _;
    let named = Format::of_name(path).ok_or(Refused::NotImage)?;
    let (file, opened) = open_regular(path)?;
    let cap = crate::protocol::FILE_PREVIEW_CAP;
    ensure!(opened.len() <= cap, Refused::TooLarge);
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    // A file that grew after its length was read is refused too, never cut short.
    ensure!(bytes.len() as u64 <= cap, Refused::TooLarge);
    let format = Format::of_header(&bytes)
        .filter(|format| *format == named)
        .ok_or(Refused::NotImage)?;
    Ok(json!({
        "mime": format.mime(),
        "size": bytes.len() as u64,
        "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
    }))
}

/// Opens the regular file the canonical `path` names, and only that file: the open does not
/// follow a link put in place of the last component, a special file is refused before it is read,
/// and a directory above it replaced between the folder check and the open shows as another file.
/// Otherwise a link swapped in after the folder check hands out a file outside the folder.
fn open_regular(path: &Path) -> crate::Result<(std::fs::File, std::fs::Metadata)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A pipe put in its place must not block before the opened-file check refuses it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|_| Refused::Missing)?;
    let opened = file.metadata().map_err(|_| Refused::Missing)?;
    ensure!(opened.is_file(), Refused::NotImage);
    let unchanged = std::fs::canonicalize(path).is_ok_and(|again| again == path)
        && same_file(&file, &opened, path);
    ensure!(unchanged, Refused::Missing);
    Ok((file, opened))
}

#[cfg(unix)]
fn same_file(_: &std::fs::File, opened: &std::fs::Metadata, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .is_ok_and(|current| (current.dev(), current.ino()) == (opened.dev(), opened.ino()))
}

#[cfg(not(unix))]
fn same_file(file: &std::fs::File, _: &std::fs::Metadata, path: &Path) -> bool {
    let opened = file.try_clone().and_then(same_file::Handle::from_file);
    opened.is_ok_and(|opened| {
        same_file::Handle::from_path(path).is_ok_and(|current| current == opened)
    })
}

#[cfg(test)]
mod tests;
