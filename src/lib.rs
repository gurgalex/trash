use chrono;
use chrono::NaiveDateTime;
use ini::Ini;
use percent_encoding::{percent_decode, percent_encode, DEFAULT_ENCODE_SET};
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use xdg;

#[derive(Debug)]
pub struct TrashInfo {
    /// Internal filename used in trashcan
    pub internal_filename: OsString,
    /// Path of file that is going to the trash
    pub path: OsString,
    /// Time file started to move to trash
    pub deletion_date: NaiveDateTime,
}

impl TrashInfo {
    pub fn new(internal: OsString, path: OsString) -> Self {
        let deletion_date = chrono::Local::now().naive_local();
        Self {
            internal_filename: internal,
            path,
            deletion_date,
        }
    }

    pub fn with_delete_datetime(
        internal: OsString,
        path: OsString,
        deletion_date: NaiveDateTime,
    ) -> Self {
        Self {
            internal_filename: internal,
            path,
            deletion_date,
        }
    }

    pub fn from_filename_and_content(
        filename: OsString,
        content: &str,
    ) -> Result<Self, ParseTrashInfoError> {
        use std::os::unix::ffi::OsStringExt;

        let res = Ini::load_from_str(content)?;
        let section = res
            .section(Some("Trash Info"))
            .ok_or(ParseTrashInfoError::MissingSection)?;
        let path = section.get("Path").ok_or(ParseTrashInfoError::MissingKey)?;
        // Credit to stephaneyfx on the Rust Discord for decoding non-utf8 percent encoded bytes

        let path = percent_decode(path.as_bytes())
            .if_any()
            .map_or(Cow::Borrowed(path.as_bytes()), Cow::Owned);
        let path = OsString::from_vec(path.into_owned());
        let deletion_datetime = section
            .get("DeletionDate")
            .ok_or(ParseTrashInfoError::MissingKey)?;
        let deletion_datetime = NaiveDateTime::from_str(deletion_datetime).unwrap();
        Ok(TrashInfo::with_delete_datetime(
            filename,
            path,
            deletion_datetime,
        ))
    }

    /// Writes info to retrieve deleted file
    fn write_infofile(&self, file: &mut File) {
        let mut info = Ini::new();
        // To aid in non-utf8 strings and to comply with spec
        // All OsStrings are url encoded

        let percent_path = percent_encode(self.path.as_bytes(), DEFAULT_ENCODE_SET).to_string();

        let deletion_datetime = self.deletion_date.format("%Y-%m-%dT%H:%M:%S").to_string();
        info.with_section(Some("Trash Info".to_owned()))
            .set("Path", percent_path)
            .set("DeletionDate", deletion_datetime);
        info.write_to(file).unwrap();
    }
}

#[derive(Debug)]
pub enum ParseTrashInfoError {
    MissingSection,
    MissingKey,
    MissingValue,
    ParseError(ini::ini::ParseError),
}

impl From<ini::ini::ParseError> for ParseTrashInfoError {
    fn from(item: ini::ini::ParseError) -> Self {
        ParseTrashInfoError::ParseError(item)
    }
}

/// Given a path attempt to reserve a trashinfo file in the $trash/info directory
fn reserve_filename<P>(path: P) -> Result<(File, PathBuf), std::io::Error>
where
    P: AsRef<Path>,
{
    let base_dirs = xdg::BaseDirectories::new().unwrap();
    let xdg_data_home = base_dirs.get_data_home();

    let trash_dir = xdg_data_home.join("Trash");
    let info_dir = PathBuf::from("info");

    let base_file = path.as_ref().file_name().expect("Empty path supplied");
    let mut filename = OsString::from(base_file);
    let info_filename_ext = OsStr::new(".trashinfo");
    filename.push(info_filename_ext);

    let mut info_path = [
        trash_dir.as_os_str(),
        info_dir.as_os_str(),
        filename.as_os_str(),
    ]
    .iter()
    .collect::<PathBuf>();

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&info_path);

    let mut duplicates = 1u32;
    loop {
        match file.as_ref() {
            Ok(_) => break,
            Err(e) => match e.kind() {
                ErrorKind::AlreadyExists => {
                    duplicates += 1;
                    // Clear existing filename
                    filename.clear();
                    filename.push(&base_file);
                    filename.push(".");
                    let s_dup = duplicates.to_string();
                    let s_dup: OsString = s_dup.into();
                    filename.push(s_dup);
                    filename.push(".trashinfo");

                    info_path.set_file_name(&filename);
                    // try again
                    file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&info_path);
                }
                ErrorKind::NotFound => {
                    // try to create trash directory in user home dir
                    std::fs::create_dir_all(&trash_dir.join(PathBuf::from(&info_dir)))
                        .unwrap_or_else(|e| {
                            panic!("failed to create home trash dir: {:?}, {:?}", &trash_dir, e)
                        });

                    // try again
                    file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&info_path);
                }
                _ => {
                    break;
                }
            },
        }
    }
    match file {
        Ok(f) => {
            let p = PathBuf::from(&info_path);
            Ok((f, p))
        }
        Err(e) => Err(e),
    }
}

/// Info and trashed location of file
#[derive(Debug)]
pub struct TrashFiles {
    /// Trashed file location
    pub trash_file: PathBuf,
    /// Info file location
    pub info_file: PathBuf,
}

impl TrashFiles {
    /// Group together internal trash info and trash file locations
    pub fn new(trash_file: PathBuf, info_file: PathBuf) -> Self {
        Self {
            trash_file,
            info_file,
        }
    }
}

/// Moves a file or directory to freedesktop.org trash spec folder
/// Returns the internal path where the file is moved to in the trash
/// Do not rely on the file still being there, as the trash item may
/// have been deleted or restored.
pub fn move_to_trash<P: AsRef<Path>>(path: P) -> Result<TrashFiles, fs_extra::error::Error> {
    let (mut info_file, info_file_name) = reserve_filename(&path)?;
    let internal_filename_for_trash = info_file_name.file_stem().unwrap();

    let trash_info = TrashInfo::new(
        internal_filename_for_trash.to_os_string(),
        path.as_ref().canonicalize().unwrap().into_os_string(),
    );
    trash_info.write_infofile(&mut info_file);

    let base_dirs = xdg::BaseDirectories::new().unwrap();
    let xdg_data_home = base_dirs.get_data_home();

    let trash_dir = xdg_data_home.join("Trash");
    let trash_dir_store_files = trash_dir.join("files");
    let trash_dest_file = trash_dir_store_files.join(internal_filename_for_trash);

    /// Helper funciton to move a file or directory to trash
    fn move_to_trash_decision(
        src_path: &Path,
        dest_path: &Path,
    ) -> Result<u64, fs_extra::error::Error> {
        if src_path.is_dir() {
            let mut copy_options = fs_extra::dir::CopyOptions::new();
            copy_options.overwrite = false;
            copy_options.skip_exist = false;
            fs_extra::dir::move_dir(src_path, dest_path, &copy_options)
        } else {
            let mut copy_options = fs_extra::file::CopyOptions::new();
            copy_options.overwrite = false;
            copy_options.skip_exist = false;

            fs_extra::file::move_file(src_path, dest_path, &copy_options)
        }
    }

    let res = move_to_trash_decision(path.as_ref(), &trash_dest_file);
    let failed_move = if let Err(e) = res {
        e
    } else {
        return Ok(TrashFiles::new(trash_dest_file, info_file_name));
    };

    use fs_extra::error::ErrorKind as fse_ErrorKind;
    let retried_res = match failed_move.kind {
        fse_ErrorKind::NotFound => {
            // The directory for storing files/dirs in trash may not exist
            create_dir_all(trash_dir_store_files).expect("failed to create trash files dir");
            // retry moving to trash
            move_to_trash_decision(path.as_ref(), &trash_dest_file)
        }
        // Fail on any other error such as permission denied or fs error
        _ => Err(failed_move),
    };

    // If moving to trash still failed, give up and return the
    // underlying error
    if let Err(e) = retried_res {
        Err(e)
    } else {
        // Everything went okay otherwise
        Ok(TrashFiles::new(trash_dest_file, info_file_name))
    }
}

#[cfg(test)]
mod tests {
    use crate::reserve_filename;
    use crate::{move_to_trash, TrashInfo};
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use tempfile::tempdir;

    /*
    #[test]
    fn test_it_works() {
        use std::fs::File;
        use std::path::PathBuf;

        let filename = "parse_string.py.trashinfo";
        let info = PathBuf::from(filename);
        let mut f = File::open(info).unwrap();
        let mut parsed = String::new();
        f.read_to_string(&mut parsed).unwrap();
        let filename = std::ffi::OsString::from(filename);
        let trashinfo = TrashInfo::from_filename_and_content(filename, &parsed).unwrap();
        assert_eq!(2 + 2, 4);
    }
    */

    #[test]
    fn test_path_creation_no_existing() {
        let temp_dir = tempdir().expect("temp dir creation failed");

        std::env::set_var("XDG_DATA_HOME", temp_dir.path().as_os_str());
        let p = PathBuf::from("test.txt");
        let info_file = reserve_filename(p.as_path());
        let filename = info_file
            .map_err(|e| format!("Failed to create file: {:?}", e))
            .unwrap();
        let mut answer = PathBuf::new();
        answer.push(&temp_dir);
        answer.push("Trash");
        answer.push("info");
        answer.push("test.txt.trashinfo");
        temp_dir.close().unwrap();
        assert_eq!(filename.1, answer);
    }

    #[test]
    fn test_full_trash() {
        use std::os::unix::ffi::OsStringExt;
        let file_dir = tempdir().expect("temp dir creation failed");
        let temp_xdg_data_home = tempdir().expect("temp dir creation failed");

        std::env::set_var("XDG_DATA_HOME", temp_xdg_data_home.path().as_os_str());
        let bytes = b"tras \xff\xee\xef\xced d h.txt";
        let bytes_for_filename = OsString::from_vec(bytes.to_vec());
        let filename = PathBuf::from(bytes_for_filename);
        let file_path = [file_dir.path(), filename.as_path()]
            .iter()
            .collect::<PathBuf>();
        {
            let mut f = std::fs::File::create(&file_path)
                .expect(&format!("Failed to create '{:?}'", file_path));
            f.write(b"hello\n").unwrap();
        }

        let res = move_to_trash(&file_path).unwrap();

        // Check that the trash file and info file are as expected
        let mut content = String::new();
        std::fs::File::open(&res.trash_file)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();

        let mut info_content = String::new();
        std::fs::File::open(&res.info_file)
            .expect(&format!("file: {:?} does not exist", res.info_file))
            .read_to_string(&mut info_content)
            .unwrap();
        let trash_info =
            TrashInfo::from_filename_and_content(res.info_file.into_os_string(), &info_content)
                .unwrap();

        // drop temp files before check
        temp_xdg_data_home.close().unwrap();
        file_dir.close().unwrap();

        assert_eq!(content, "hello\n");
        assert_eq!(trash_info.path, file_path);
    }
}
