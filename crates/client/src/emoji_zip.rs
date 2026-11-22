//! Reading a .zip of pictures for "Import pack": nothing is ever written to disk, only
//! the file name of each picture is used, and the archive is treated as hostile.

use std::io::{Cursor, Read};

use zip::ZipArchive;

/// Entries (of any kind) an archive may hold before it is refused without being read.
const MAX_ENTRIES: usize = 2_000;
/// One picture, uncompressed.
const MAX_ENTRY: u64 = 4 * 1024 * 1024;
/// All pictures together, uncompressed: a zip bomb stops here.
const MAX_TOTAL: u64 = 48 * 1024 * 1024;
/// The archive file itself.
pub const MAX_ARCHIVE: u64 = 32 * 1024 * 1024;
/// The longest name an emoji can have.
const MAX_NAME: usize = 32;

/// A picture from the archive, and the name its file suggests.
pub struct Picture {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Why an archive was refused; the import dialog says it in plain words.
#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    NotZip,
    /// More entries than a pack file reasonably holds.
    TooManyFiles,
    /// More pictures than the server has room for.
    TooManyPictures(usize),
    /// A picture or the whole archive unpacks to more than the limits.
    TooBig,
    NoPictures,
}

/// The pictures in `archive`, at most `max` of them, named from their file names.
/// Folders, hidden files, anything that is not a PNG, JPEG, GIF or WebP name, and any
/// path that climbs out (`..`) or is absolute are skipped, never followed.
pub fn read(archive: &[u8], max: usize) -> Result<Vec<Picture>, Problem> {
    if archive.len() as u64 > MAX_ARCHIVE {
        return Err(Problem::TooBig);
    }
    let mut zip = ZipArchive::new(Cursor::new(archive)).map_err(|_| Problem::NotZip)?;
    if zip.len() > MAX_ENTRIES {
        return Err(Problem::TooManyFiles);
    }
    let mut pictures: Vec<Picture> = Vec::new();
    let mut total = 0u64;
    for index in 0..zip.len() {
        let Ok(file) = zip.by_index(index) else {
            continue;
        };
        let Ok(name) = file.name().map(|name| name.into_owned()) else {
            continue;
        };
        if file.is_dir() || !safe_path(&name) {
            continue;
        }
        let Some(stem) = picture_stem(&name) else {
            continue;
        };
        // The declared size can lie; it only saves reading what is certainly too big.
        if file.size() > MAX_ENTRY {
            return Err(Problem::TooBig);
        }
        let mut bytes = Vec::new();
        file.take(MAX_ENTRY + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Problem::NotZip)?;
        total += bytes.len() as u64;
        if bytes.len() as u64 > MAX_ENTRY || total > MAX_TOTAL {
            return Err(Problem::TooBig);
        }
        if pictures.len() == max {
            return Err(Problem::TooManyPictures(max));
        }
        pictures.push(Picture {
            name: sanitize(&stem),
            bytes,
        });
    }
    if pictures.is_empty() {
        return Err(Problem::NoPictures);
    }
    // Names are unique inside the pack: the second `party` becomes `party_2`.
    let mut taken: Vec<String> = Vec::new();
    for picture in &mut pictures {
        picture.name = unique(&picture.name, &taken);
        taken.push(picture.name.clone());
    }
    Ok(pictures)
}

/// A relative path that stays inside the archive: no `..`, no leading `/` or `\`, no
/// drive letter, no NUL.
fn safe_path(name: &str) -> bool {
    let name = name.replace('\\', "/");
    !name.starts_with('/')
        && !name.contains('\0')
        && !name.split('/').any(|part| part == "..")
        && !name.as_bytes().get(1).is_some_and(|c| *c == b':')
}

/// The file name without folders or extension, when it names a picture a person made:
/// not hidden, not macOS resource-fork junk.
fn picture_stem(name: &str) -> Option<String> {
    let name = name.replace('\\', "/");
    if name.split('/').any(|part| part == "__MACOSX") {
        return None;
    }
    let file = name.rsplit('/').next()?;
    if file.starts_with('.') {
        return None;
    }
    let (stem, extension) = file.rsplit_once('.')?;
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "gif" | "jpg" | "jpeg" | "webp"
    )
    .then(|| stem.to_owned())
}

/// A file name as an emoji name: lowercase letters, digits and `_`, 2 to 32 of them.
pub fn sanitize(stem: &str) -> String {
    let mut name = crate::emoji::clean_name(stem);
    if name.len() < 2 {
        name.push_str("_emoji");
    }
    name.truncate(MAX_NAME);
    name
}

/// `name`, or `name_2`, `name_3`… when `taken` has it (compared case-insensitively),
/// still within the name length.
pub fn unique(name: &str, taken: &[String]) -> String {
    let free = |candidate: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(candidate));
    if free(name) {
        return name.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!("_{n}");
            let keep = MAX_NAME - suffix.len();
            format!("{}{suffix}", &name[..name.len().min(keep)])
        })
        .find(|candidate| free(candidate))
        .expect("an unused number exists")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn names_come_from_files_and_folders_and_junk_are_skipped() {
        let bytes = archive(&[
            ("party.png", b"1"),
            ("sub/Wave Hand.GIF", b"2"),
            ("sub/", b""),
            ("__MACOSX/._party.png", b"x"),
            (".hidden.png", b"x"),
            ("notes.txt", b"x"),
            ("noextension", b"x"),
        ]);
        let pictures = read(&bytes, 10).unwrap();
        let names: Vec<_> = pictures.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["party", "wave_hand"]);
    }

    #[test]
    fn paths_that_climb_out_are_ignored() {
        let bytes = archive(&[
            ("../evil.png", b"x"),
            ("a/../../evil2.png", b"x"),
            ("/etc/evil3.png", b"x"),
            ("C:/evil4.png", b"x"),
            ("..\\evil5.png", b"x"),
            ("fine.png", b"ok"),
        ]);
        let pictures = read(&bytes, 10).unwrap();
        assert_eq!(pictures.len(), 1);
        assert_eq!(pictures[0].name, "fine");
        assert_eq!(pictures[0].bytes, b"ok");
    }

    #[test]
    fn too_many_pictures_and_files_are_refused() {
        let files: Vec<(String, &[u8])> =
            (0..5).map(|n| (format!("e{n}.png"), &b"x"[..])).collect();
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        let bytes = archive(&refs);
        assert_eq!(read(&bytes, 4).err(), Some(Problem::TooManyPictures(4)));
        assert_eq!(read(&bytes, 5).unwrap().len(), 5);
        // Entries that are not pictures still count against the file limit.
        let junk: Vec<(String, &[u8])> = (0..=MAX_ENTRIES)
            .map(|n| (format!("f{n}.txt"), &b""[..]))
            .collect();
        let refs: Vec<(&str, &[u8])> = junk.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        assert_eq!(read(&archive(&refs), 10).err(), Some(Problem::TooManyFiles));
    }

    #[test]
    fn zip_bombs_are_refused_by_what_they_unpack_to() {
        // Compresses to almost nothing, unpacks to more than one picture may be.
        let big = vec![0u8; MAX_ENTRY as usize + 1];
        assert_eq!(
            read(&archive(&[("big.png", &big)]), 10).err(),
            Some(Problem::TooBig)
        );
        // Many pictures that are each fine but together too big.
        let each = vec![0u8; MAX_ENTRY as usize];
        let count = (MAX_TOTAL / MAX_ENTRY) as usize + 1;
        let names: Vec<String> = (0..count).map(|n| format!("p{n}.png")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &each[..])).collect();
        assert_eq!(read(&archive(&files), 100).err(), Some(Problem::TooBig));
    }

    #[test]
    fn archives_that_are_not_zips_or_hold_no_pictures_are_refused() {
        assert_eq!(read(b"not a zip", 10).err(), Some(Problem::NotZip));
        assert_eq!(
            read(&archive(&[("readme.txt", b"hi")]), 10).err(),
            Some(Problem::NoPictures)
        );
    }

    #[test]
    fn names_are_made_valid_and_unique() {
        assert_eq!(sanitize("Party Parrot!"), "party_parrot");
        assert_eq!(sanitize("a"), "a_emoji");
        assert_eq!(sanitize("日本語"), "_emoji");
        assert_eq!(sanitize(&"x".repeat(60)).len(), 32);
        assert_eq!(sanitize("my-cool.emoji"), "my_coolemoji");
        let taken = vec!["party".to_owned(), "PARTY_2".to_owned()];
        assert_eq!(unique("party", &taken), "party_3");
        assert_eq!(unique("wave", &taken), "wave");
        let long = "y".repeat(32);
        let unique_long = unique(&long, std::slice::from_ref(&long));
        assert_eq!(unique_long.len(), 32);
        assert!(unique_long.ends_with("_2"));
        // Two files with the same name in different folders.
        let bytes = archive(&[("a/party.png", b"1"), ("b/party.png", b"2")]);
        let names: Vec<_> = read(&bytes, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["party", "party_2"]);
    }
}
