<sub><i>❗ This project was retroactively uploaded. Minimal code changes were made.</i></sub>

# 🗄️ Windows backup FUSE Filesystem 🏆

This project is a proof of concept and my humble attempt at creating a FUSE filesystem for mounting Windows 7 disk backups. It allows browsing the backup file structure and reading file contents without unpacking the entire backup to your disk (and without installing Windows).

I created it out of pure necessity: I've had an old backup from Windows 7 that I wanted to browse through, and to my surprise there weren't any tools to do that on linux. So, I decided to some have fun and write the FUSE filesystem for that myself.

## Limitations 🚭

I decided not to reverse engineer the `.wbcat` binary format that describes the backup. I don't even know what's stored in it. Maybe there is some very important data (there probably is), and maybe without it the backup is reassembled incorrectly (it probably is), but it was outside the scope of this project.

If you really need the data inside the `.wbcat`, and you have a clue on how to reverse engineer this format, you can open an issue and maybe I will be able to add the support for it.

## Features 🚀

- **File Attributes**: Retrieves basic file attributes such as size and timestamp.
- **Caching**: WinBackup-fuse implements **basic** caching mechanisms to speed up file access.
- **Relatively Low Memory Usage**: I've been able to mount an entire disk backup (~150 GB), while using <5 GB of RAM for storing the file system.

## Usage 🛠️

1. **Build**: Clone this repository and build the binary using `cargo`.
2. **Run**: Execute the built binary with the glob to all the backup archives as the first argument and the desired mount point as the second argument.

### Example:

```bash
./winbackup-fuse '/path/to/backup/**/*.zip' /mnt/winbackup
```
