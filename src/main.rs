#![feature(entry_insert)]

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, Duration, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use indicatif::ProgressIterator;
use libc::ENOENT;
use ouroboros::self_referencing;
use zip::ZipArchive;
use zip::read::ZipFile;


const BLOCK_SIZE: u64 = 64 * 1024;
const TTL: Duration = Duration::from_secs(60 * 60 * 24); // Just a big number


#[derive(Debug)]
struct FileInfo {
    time: SystemTime,
    size: u64,
}

#[derive(Debug)]
struct File {
    file_id: usize,
    path: String,
    info: FileInfo,
}

#[derive(Debug)]
struct Source<'a> {
    archive: &'a Path,
    file_id: usize,
}

#[derive(Debug)]
struct FSEntryInfo<'a> {
    info: FileInfo,
    source: Source<'a>,
}

#[derive(Debug)]
enum FSEntry<'a> {
    File {
        ino: u64,
        info: FSEntryInfo<'a>,
    },
    Directory {
        ino: u64,
        entries: HashMap<String, FSEntry<'a>>,
    },
}

impl FSEntry<'_> {
    fn dir(ino: u64) -> Self {
        Self::Directory {
            ino,
            entries: HashMap::new(),
        }
    }
}

struct WinbackupTreeBuilder {
    filename_encoding: &'static encoding_rs::Encoding,
}

impl WinbackupTreeBuilder {
    fn decode_filename(&self, input: &[u8]) -> String {
        let (string, _, _) = self.filename_encoding.decode(input);
        string.to_string()
    }

    fn rough_file_info(&self, path: &Path) -> Result<Vec<File>, ()> {
        let file = fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).map_err(|_| ())?;

        let mut files = vec![];

        for i in 0..archive.len() {
            let file = archive.by_index(i).unwrap();

            if file.is_dir() {
                continue;
            }

            let name = self.decode_filename(file.name_raw());
            let time = file.last_modified().to_time().unwrap().into();

            files.push(File {
                file_id: i,
                path: name,
                info: FileInfo {
                    time,
                    size: file.size(),
                },
            });
        }

        Ok(files)
    }

    fn parse_multiple_archives<'a>(&self, sources: &'a [PathBuf]) -> FSEntry<'a> {
        let mut files: HashMap<String, Vec<FSEntryInfo>> = HashMap::new();

        for archive in sources.iter().progress() {
            let Ok(infos) = self.rough_file_info(archive) else {
                continue
            };

            for file in infos {
                let entry = FSEntryInfo {
                    info: file.info,
                    source: Source {
                        archive,
                        file_id: file.file_id,
                    },
                };

                if let Some(f) = files.get_mut(&file.path) {
                    f.push(entry);
                } else {
                    files.insert(file.path, vec![entry]);
                }
            }
        }

        let mut root = FSEntry::dir(1);

        let mut ino_counter = 2;
        for (path, info) in files {
            let Some(info) = info.into_iter().max_by_key(|x| x.info.time) else { continue };

            let Some((path, name)) = path.rsplit_once('\\') else { continue };

            let mut prev_dir = &mut root;

            for dir_name in path.split('\\') {
                let FSEntry::Directory{ entries, .. } = prev_dir else { break };

                if !entries.contains_key(dir_name) {
                    entries.insert(dir_name.to_owned(), FSEntry::dir(ino_counter));
                    ino_counter += 1;
                }

                prev_dir = entries.get_mut(dir_name).unwrap();
            }

            let FSEntry::Directory{ entries, .. } = prev_dir else { continue };
            entries.insert(
                name.to_owned(),
                FSEntry::File {
                    ino: ino_counter,
                    info,
                },
            );
            ino_counter += 1;
        }

        root
    }
}

fn build_filesystem_map<'a>(root: &'a FSEntry, ino_to_entry: &mut HashMap<u64, &'a FSEntry<'a>>) {
    match root {
        FSEntry::Directory { ino, entries, .. } => {
            ino_to_entry.insert(*ino, root);
            for entry in entries.values() {
                build_filesystem_map(entry, ino_to_entry);
            }
        }
        FSEntry::File { ino, .. } => {
            ino_to_entry.insert(*ino, root);
        }
    }
}

impl FSEntry<'_> {
    fn attrs(&self) -> FileAttr {
        match self {
            FSEntry::File { ino, info, .. } => FileAttr {
                ino: *ino,
                size: info.info.size,
                blocks: (info.info.size + BLOCK_SIZE - 1) / BLOCK_SIZE,
                atime: info.info.time,
                mtime: info.info.time,
                ctime: info.info.time,
                crtime: info.info.time,
                kind: FileType::RegularFile,
                perm: 0o644,
                nlink: 1,
                uid: 501,
                gid: 20,
                rdev: 0,
                flags: 0,
                blksize: BLOCK_SIZE as u32,
            },
            FSEntry::Directory { ino, .. } => FileAttr {
                ino: *ino,
                size: 0,
                blocks: 0,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 501,
                gid: 20,
                rdev: 0,
                flags: 0,
                blksize: 0, // ?
            },
        }
    }

    fn filetype(&self) -> FileType {
        match self {
            FSEntry::File { .. } => FileType::RegularFile,
            FSEntry::Directory { .. } => FileType::Directory,
        }
    }
}

#[derive(Debug)]
struct OpenedArchive {
    offset: usize,
    content: OwnedZipFileBytes,
}

impl OpenedArchive {
    pub fn open(source: &Source) -> Self {
        Self {
            offset: 0,
            content: OwnedZipFileBytes::open(source.archive, source.file_id),
        }
    }

    pub fn is_viable(&self, offset: usize) -> bool {
        offset >= self.offset
    }

    pub fn read_bytes(&mut self, offset: usize, size: usize) -> Vec<u8> {
        let to_skip = offset - self.offset;
        self.offset = offset + size;

        self.content.with_bytes_mut(|bytes| {
            bytes
                .skip(to_skip)
                .take(size)
                .filter_map(Result::ok)
                .collect()
        })
    }
}

impl std::fmt::Debug for OwnedZipFileBytes {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        Ok(())
    }
}

#[self_referencing]
struct OwnedZipFileBytes {
    archive: ZipArchive<std::fs::File>,
    #[borrows(mut archive)]
    #[not_covariant]
    bytes: std::io::Bytes<ZipFile<'this>>,
}

impl OwnedZipFileBytes {
    pub fn open(archive: &Path, file_id: usize) -> Self {
        let file = std::fs::File::open(archive).unwrap();
        let archive = ZipArchive::new(file).unwrap();
        OwnedZipFileBytesBuilder {
            archive,
            bytes_builder: |archive| archive.by_index(file_id).unwrap().bytes(),
        }
        .build()
    }
}

struct WinbackupFS<'a> {
    filesystem: HashMap<u64, &'a FSEntry<'a>>,
    handlers: HashMap<u64, OpenedArchive>,
    handlers_counter: u64,
}

impl<'a> WinbackupFS<'a> {
    fn from_tree(root: &'a FSEntry<'a>) -> Self {
        let mut ino_to_entry: HashMap<u64, &FSEntry> = HashMap::new();
        build_filesystem_map(root, &mut ino_to_entry);

        WinbackupFS {
            filesystem: ino_to_entry,
            handlers: HashMap::new(),
            handlers_counter: 0,
        }
    }
}

impl Filesystem for WinbackupFS<'_> {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(FSEntry::Directory{ entries, .. }) = self.filesystem.get(&parent) else {
            reply.error(ENOENT);
            return;
        };

        let Some(name) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };

        let Some(file) = entries.get(name) else {
            reply.error(ENOENT);
            return;
        };

        reply.entry(&TTL, &file.attrs(), 0);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match self.filesystem.get(&ino) {
            Some(entry) => reply.attr(&TTL, &entry.attrs()),
            _ => reply.error(ENOENT),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handlers.remove(&fh);
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(self.handlers_counter, 0);
        self.handlers_counter += 1;
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        let archive = match self.handlers.entry(fh) {
            Entry::Occupied(entry) if entry.get().is_viable(offset as usize) => {
                &mut *entry.into_mut()
            }
            entry => {
                let archive = {
                    let Some(FSEntry::File{ info, .. }) = self.filesystem.get(&ino) else {
                        reply.error(ENOENT);
                        return;
                    };

                    OpenedArchive::open(&info.source)
                };

                entry.insert_entry(archive).into_mut()
            }
        };

        let bytes = archive.read_bytes(offset as usize, size as usize);
        reply.data(&bytes);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(FSEntry::Directory{ entries, .. }) = self.filesystem.get(&ino) else {
            reply.error(ENOENT);
            return;
        };

        for (i, (name, entry)) in entries.iter().enumerate().skip(offset as usize) {
            let ino = match entry {
                FSEntry::File { ino, .. } | FSEntry::Directory { ino, .. } => ino,
            };

            if reply.add(*ino, (i + 1) as i64, entry.filetype(), name) {
                break;
            }
        }

        reply.ok();
    }
}

fn main() {
    let archives_glob = env::args().nth(1).expect("Provide a glob for the backup archives list");
    let mountpoint = env::args().nth(2).expect("Provide a mount point");

    let sources: Vec<PathBuf> = glob::glob(&archives_glob)
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    let winbackup = WinbackupTreeBuilder {
        filename_encoding: encoding_rs::IBM866,
    };

    let tree = winbackup.parse_multiple_archives(&sources);
    let fs = WinbackupFS::from_tree(&tree);

    let options = vec![MountOption::RO, MountOption::FSName("winbackup-fuse".to_string())];
    fuser::mount2(fs, mountpoint, &options).unwrap();
}
