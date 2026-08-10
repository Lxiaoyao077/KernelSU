//! Magic mount: bind-mount module engine (ported from FolkPatch).
//! Modules opt in via a `.magic_mount` marker; others keep their own mounting.

use std::{
    cmp::PartialEq,
    collections::{HashMap, hash_map::Entry},
    ffi::CStr,
    fs,
    fs::{DirEntry, FileType, create_dir, create_dir_all, read_dir, read_link},
    os::unix::fs::{FileTypeExt, symlink},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use extattr::lgetxattr;
use rustix::{
    fs::{Gid, MetadataExt, Mode, Uid, chmod, chown},
    mount::{
        MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_bind, mount_change,
        mount_move, unmount,
    },
};

use self::NodeFileType::{Directory, RegularFile, Symlink, Whiteout};
use crate::{
    defs::{
        DISABLE_FILE_NAME, MAGIC_MOUNT_MARK_FILE, MAGIC_MOUNT_SOURCE, MODULE_DIR, REMOVE_FILE_NAME,
        SKIP_MOUNT_FILE_NAME,
    },
    restorecon::{lgetfilecon, lsetfilecon},
};

const REPLACE_DIR_FILE_NAME: &str = ".replace";
const REPLACE_DIR_XATTR: &str = "trusted.overlay.opaque";

/// Max module tree depth to guard against stack overflow on pathological trees.
const MAX_MODULE_DEPTH: u32 = 128;
/// tmpfs size limit for the mount skeleton workspace.
const TMPFS_SIZE: &CStr = c"size=16M,mode=755";

/// Partitions that may be relocated from /system/<name> to /<name> on devices
/// where the real /system/<name> is a symlink (or, for `odm`/`oem`, always).
const BUILTIN_PARTITIONS: [(&str, bool); 5] = [
    ("vendor", true),
    ("system_ext", true),
    ("product", true),
    ("odm", false),
    ("oem", false),
];

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
enum NodeFileType {
    RegularFile,
    Directory,
    Symlink,
    Whiteout,
}

impl NodeFileType {
    fn from_file_type(file_type: FileType) -> Self {
        if file_type.is_file() {
            RegularFile
        } else if file_type.is_dir() {
            Directory
        } else if file_type.is_symlink() {
            Symlink
        } else {
            Whiteout
        }
    }

    /// Whether mounting this node over `real_path` needs a tmpfs overlay.
    fn needs_tmpfs_vs_real(&self, real_path: &Path) -> bool {
        match self {
            Symlink => true,
            Whiteout => real_path.exists(),
            _ => real_path.symlink_metadata().map_or(true, |metadata| {
                let real_type = Self::from_file_type(metadata.file_type());
                real_type != *self || real_type == Symlink
            }),
        }
    }
}

#[derive(Debug, Clone)]
struct Node {
    name: String,
    file_type: NodeFileType,
    children: HashMap<String, Self>,
    // the module that owned this node
    module_path: Option<PathBuf>,
    replace: bool,
    skip: bool,
}

impl Node {
    fn collect_module_files<P>(&mut self, module_dir: P) -> Result<bool>
    where
        P: AsRef<Path>,
    {
        self.collect_module_files_depth(module_dir, 0)
    }

    fn collect_module_files_depth<P>(&mut self, module_dir: P, depth: u32) -> Result<bool>
    where
        P: AsRef<Path>,
    {
        if depth > MAX_MODULE_DEPTH {
            log::warn!(
                "module tree too deep at {}, stop collecting",
                module_dir.as_ref().display()
            );
            return Ok(false);
        }

        let dir = module_dir.as_ref();
        let mut has_file = false;
        for entry in dir.read_dir()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();

            // The `.replace` marker only tags its parent directory; it must
            // not end up mounted as a real file.
            if name == REPLACE_DIR_FILE_NAME {
                continue;
            }

            let node = match self.children.entry(name.clone()) {
                Entry::Occupied(o) => Some(o.into_mut()),
                Entry::Vacant(v) => Self::new_module(&name, &entry).map(|it| v.insert(it)),
            };

            if let Some(node) = node {
                has_file |= if node.file_type == NodeFileType::Directory {
                    node.collect_module_files_depth(dir.join(&node.name), depth + 1)?
                        || node.replace
                } else {
                    true
                }
            }
        }

        Ok(has_file)
    }

    fn dir_is_replace<P>(path: P) -> bool
    where
        P: AsRef<Path>,
    {
        if let Ok(v) = lgetxattr(&path, REPLACE_DIR_XATTR)
            && String::from_utf8_lossy(&v) == "y"
        {
            return true;
        }

        path.as_ref().join(REPLACE_DIR_FILE_NAME).exists()
    }

    fn new_root(name: &str) -> Self {
        Self {
            name: name.to_string(),
            file_type: Directory,
            children: HashMap::default(),
            module_path: None,
            replace: false,
            skip: false,
        }
    }

    fn new_module(name: &str, entry: &DirEntry) -> Option<Self> {
        // file_type() avoids following symlinks, keeping them as Symlink nodes.
        let file_type = match entry.file_type() {
            Ok(ft) if ft.is_char_device() => {
                let is_whiteout = entry.metadata().is_ok_and(|m| m.rdev() == 0);
                if is_whiteout {
                    NodeFileType::Whiteout
                } else {
                    NodeFileType::from_file_type(ft)
                }
            }
            Ok(ft) => NodeFileType::from_file_type(ft),
            Err(_) => return None,
        };
        let path = entry.path();
        let replace = file_type == NodeFileType::Directory && Self::dir_is_replace(&path);
        if replace {
            log::debug!("{} need replace", path.display());
        }
        Some(Self {
            name: name.to_string(),
            file_type,
            children: HashMap::default(),
            module_path: Some(path),
            replace,
            skip: false,
        })
    }
}

fn collect_module_files() -> Result<Option<Node>> {
    let mut root = Node::new_root("");
    let mut system = Node::new_root("system");
    let module_root = Path::new(MODULE_DIR);
    let mut has_file = false;

    if !module_root.is_dir() {
        log::info!(
            "module dir {} not present, skip magic mount",
            module_root.display()
        );
        return Ok(None);
    }

    log::debug!("begin collect module files: {}", module_root.display());

    for entry in module_root.read_dir()?.flatten() {
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            log::warn!(
                "skip module with non-UTF8 name: {}",
                entry.file_name().to_string_lossy()
            );
            continue;
        };
        log::debug!("processing new module: {id}");

        if !entry.path().join("module.prop").exists() {
            log::debug!("skipped module {id}, because not found module.prop");
            continue;
        }

        if entry.path().join(DISABLE_FILE_NAME).exists()
            || entry.path().join(REMOVE_FILE_NAME).exists()
            || entry.path().join(SKIP_MOUNT_FILE_NAME).exists()
        {
            log::debug!("skipped module {id}, due to disable/remove/skip_mount");
            continue;
        }

        // Skip modules without the magic mount opt-in.
        if !entry.path().join(MAGIC_MOUNT_MARK_FILE).exists() {
            log::debug!(
                "skipped module {id}, no {MAGIC_MOUNT_MARK_FILE} marker (uses its own mounting)"
            );
            continue;
        }

        let system_dir = entry.path().join("system");
        if !system_dir.is_dir() {
            continue;
        }

        log::debug!("collecting {}", entry.path().display());

        has_file |= system.collect_module_files(system_dir)?;

        // The installer moves system/<partition> to the module root and leaves
        // a symlink behind. Collect the real partition dirs so their content
        // is mounted instead of an empty symlink clone.
        for (partition, _) in BUILTIN_PARTITIONS {
            let part_dir = entry.path().join(partition);
            if part_dir.is_dir() {
                let is_symlink_node = system
                    .children
                    .get(partition)
                    .is_some_and(|n| n.file_type == NodeFileType::Symlink);
                if is_symlink_node || !system.children.contains_key(partition) {
                    let node = Node {
                        name: partition.to_string(),
                        file_type: Directory,
                        children: HashMap::default(),
                        module_path: Some(part_dir.clone()),
                        replace: Node::dir_is_replace(&part_dir),
                        skip: false,
                    };
                    system.children.insert(partition.to_string(), node);
                }
                if let Some(node) = system.children.get_mut(partition) {
                    has_file |= node.collect_module_files(&part_dir)?;
                }
            }
        }
    }

    if has_file {
        for (partition, require_symlink) in BUILTIN_PARTITIONS {
            let path_of_root = Path::new("/").join(partition);
            let path_of_system = Path::new("/system").join(partition);
            if path_of_root.is_dir() && (!require_symlink || path_of_system.is_symlink()) {
                let name = partition.to_string();
                if let Some(node) = system.children.remove(&name) {
                    root.children.insert(name, node);
                }
            }
        }

        root.children.insert("system".to_string(), system);
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

fn clone_symlink<Src: AsRef<Path>, Dst: AsRef<Path>>(src: Src, dst: Dst) -> Result<()> {
    let src_symlink = read_link(src.as_ref())?;
    symlink(&src_symlink, dst.as_ref())?;
    lsetfilecon(dst.as_ref(), lgetfilecon(src.as_ref())?.as_str())?;
    log::debug!(
        "clone symlink {} -> {}({})",
        src.as_ref().display(),
        dst.as_ref().display(),
        src_symlink.display()
    );
    Ok(())
}

fn mount_mirror<P: AsRef<Path>, WP: AsRef<Path>>(
    path: P,
    work_dir_path: WP,
    entry: &DirEntry,
    depth: u32,
) -> Result<()> {
    let path = path.as_ref().join(entry.file_name());
    if depth > MAX_MODULE_DEPTH {
        log::warn!("mirror tree too deep at {}, stop mirroring", path.display());
        return Ok(());
    }

    let work_dir_path = work_dir_path.as_ref().join(entry.file_name());
    let file_type = entry.file_type()?;

    if file_type.is_file() {
        log::debug!(
            "mount mirror file {} -> {}",
            path.display(),
            work_dir_path.display()
        );
        fs::File::create(&work_dir_path)?;
        mount_bind(&path, &work_dir_path)?;
    } else if file_type.is_dir() {
        log::debug!(
            "mount mirror dir {} -> {}",
            path.display(),
            work_dir_path.display()
        );
        create_dir(&work_dir_path)?;
        let metadata = entry.metadata()?;
        chmod(&work_dir_path, Mode::from_raw_mode(metadata.mode()))?;
        chown(
            &work_dir_path,
            Some(Uid::from_raw(metadata.uid())),
            Some(Gid::from_raw(metadata.gid())),
        )?;
        lsetfilecon(&work_dir_path, lgetfilecon(&path)?.as_str())?;
        for entry in read_dir(&path)?.flatten() {
            mount_mirror(&path, &work_dir_path, &entry, depth + 1)?;
        }
    } else if file_type.is_symlink() {
        log::debug!(
            "create mirror symlink {} -> {}",
            path.display(),
            work_dir_path.display()
        );
        clone_symlink(&path, &work_dir_path)?;
    }

    Ok(())
}

fn should_create_tmpfs(path: &Path, current: &mut Node, has_tmpfs: bool) -> bool {
    if has_tmpfs {
        return false;
    }
    if current.replace && current.module_path.is_some() {
        return true;
    }
    for (name, node) in &mut current.children {
        let real_path = path.join(name);
        if node.file_type.needs_tmpfs_vs_real(&real_path) {
            // Nodes without a module dir (e.g. the root `system` node) can
            // still overlay an existing real path; only bail out when there
            // is no source at all. Skipping would silently drop new files,
            // new dirs and whiteouts directly under /system.
            if current.module_path.is_none() && !path.exists() {
                log::error!("cannot create tmpfs on {}, ignore: {name}", path.display());
                node.skip = true;
                continue;
            }
            return true;
        }
    }
    false
}

fn prepare_tmpfs_skeleton(
    path: &Path,
    work_dir_path: &Path,
    module_path: Option<&PathBuf>,
) -> Result<()> {
    log::debug!(
        "creating tmpfs skeleton for {} at {}",
        path.display(),
        work_dir_path.display()
    );
    create_dir_all(work_dir_path)?;
    let source: &Path = if path.exists() {
        path
    } else if let Some(mp) = module_path {
        mp
    } else {
        bail!("cannot mount root dir {}!", path.display());
    };
    let metadata = source.metadata()?;
    chmod(work_dir_path, Mode::from_raw_mode(metadata.mode()))?;
    chown(
        work_dir_path,
        Some(Uid::from_raw(metadata.uid())),
        Some(Gid::from_raw(metadata.gid())),
    )?;
    lsetfilecon(work_dir_path, lgetfilecon(source)?.as_str())?;
    Ok(())
}

/// Log a failed child mount and keep going: one broken node must not abort
/// the mounts of the remaining modules.
fn handle_mount_result(result: Result<()>, path: &Path, name: &str) -> Result<()> {
    if let Err(e) = result {
        log::error!("mount child {}/{} failed: {}", path.display(), name, e);
    }
    Ok(())
}

fn process_existing_entries(
    path: &Path,
    work_dir_path: &Path,
    children: &mut HashMap<String, Node>,
    has_tmpfs: bool,
) -> Result<()> {
    for entry in path.read_dir()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let result = if let Some(node) = children.remove(&name) {
            if node.skip {
                continue;
            }
            do_magic_mount(path, work_dir_path, node, has_tmpfs)
                .with_context(|| format!("magic mount {}/{name}", path.display()))
        } else if has_tmpfs {
            mount_mirror(path, work_dir_path, &entry, 0)
                .with_context(|| format!("mount mirror {}/{name}", path.display()))
        } else {
            Ok(())
        };
        handle_mount_result(result, path, &name)?;
    }
    Ok(())
}

fn process_remaining_children(
    path: &Path,
    work_dir_path: &Path,
    children: HashMap<String, Node>,
    has_tmpfs: bool,
) -> Result<()> {
    for (name, node) in children {
        if node.skip {
            continue;
        }
        let result = do_magic_mount(path, work_dir_path, node, has_tmpfs)
            .with_context(|| format!("magic mount {}/{name}", path.display()));
        handle_mount_result(result, path, &name)?;
    }
    Ok(())
}

fn move_tmpfs_to_target(work_dir_path: &Path, target: &Path) -> Result<()> {
    log::debug!(
        "moving tmpfs {} -> {}",
        work_dir_path.display(),
        target.display()
    );
    mount_move(work_dir_path, target).context("move self")?;
    mount_change(target, MountPropagationFlags::PRIVATE).context("make self private")?;
    Ok(())
}

fn do_magic_mount<P: AsRef<Path>, WP: AsRef<Path>>(
    path: P,
    work_dir_path: WP,
    mut current: Node,
    has_tmpfs: bool,
) -> Result<()> {
    let path = path.as_ref().join(&current.name);
    let work_dir_path = work_dir_path.as_ref().join(&current.name);
    match current.file_type {
        RegularFile => {
            let target_path = if has_tmpfs {
                fs::File::create(&work_dir_path)?;
                &work_dir_path
            } else {
                &path
            };
            if let Some(module_path) = &current.module_path {
                log::debug!(
                    "mount module file {} -> {}",
                    module_path.display(),
                    work_dir_path.display()
                );
                mount_bind(module_path, target_path)?;
            } else {
                bail!("cannot mount root file {}!", path.display());
            }
        }
        Symlink => {
            if let Some(module_path) = &current.module_path {
                log::debug!(
                    "create module symlink {} -> {}",
                    module_path.display(),
                    work_dir_path.display()
                );
                clone_symlink(module_path, &work_dir_path)?;
            } else {
                bail!("cannot mount root symlink {}!", path.display());
            }
        }
        Directory => {
            let create_tmpfs = should_create_tmpfs(&path, &mut current, has_tmpfs);
            let has_tmpfs = has_tmpfs || create_tmpfs;

            if has_tmpfs {
                prepare_tmpfs_skeleton(&path, &work_dir_path, current.module_path.as_ref())?;
            }
            if create_tmpfs {
                log::debug!(
                    "creating tmpfs for {} at {}",
                    path.display(),
                    work_dir_path.display()
                );
                mount_bind(&work_dir_path, &work_dir_path).context("bind self")?;
            }
            if path.exists() && !current.replace {
                process_existing_entries(&path, &work_dir_path, &mut current.children, has_tmpfs)?;
            }
            if current.replace {
                if current.module_path.is_none() {
                    bail!(
                        "dir {} is declared as replaced but it is root!",
                        path.display()
                    );
                }
                log::debug!("dir {} is replaced", path.display());
            }
            process_remaining_children(&path, &work_dir_path, current.children, has_tmpfs)?;
            if create_tmpfs {
                move_tmpfs_to_target(&work_dir_path, &path)?;
            }
        }
        Whiteout => {
            log::debug!("file {} is removed", path.display());
        }
    }
    Ok(())
}

/// True if `path` is itself a mount point (device differs from parent's).
fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(parent_meta) = fs::metadata(parent) else {
        return false;
    };
    meta.dev() != parent_meta.dev()
}

/// Bind-mount all modules opted into magic mount.
pub fn magic_mount() -> Result<()> {
    if let Some(root) = collect_module_files()? {
        log::debug!("collected: {root:#?}");
        let tmp_dir = PathBuf::from(MAGIC_MOUNT_SOURCE);

        // Drop a stale tmpfs so soft_reboot cannot stack mounts.
        if is_mountpoint(&tmp_dir) {
            if let Err(e) = unmount(&tmp_dir, UnmountFlags::DETACH) {
                log::warn!("stale tmpfs at {}: {e}", tmp_dir.display());
            }
            fs::remove_dir_all(&tmp_dir).ok();
        }

        fs::create_dir_all(&tmp_dir)?;
        mount(
            "tmpfs",
            &tmp_dir,
            "tmpfs",
            MountFlags::empty(),
            Some(TMPFS_SIZE),
        )
        .context("mount tmp")?;
        mount_change(&tmp_dir, MountPropagationFlags::PRIVATE).context("make tmp private")?;
        let result = do_magic_mount("/", &tmp_dir, root, false);
        // Detach and clean up even on failure to avoid residue.
        if let Err(e) = unmount(&tmp_dir, UnmountFlags::DETACH) {
            log::error!("failed to unmount tmp {e}");
        }
        fs::remove_dir_all(&tmp_dir).ok();
        result
    } else {
        log::info!("no magic mount modules, skipping!");
        Ok(())
    }
}
