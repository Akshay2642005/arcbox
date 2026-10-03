//! The sidecar module against a real directory: what the Mac writes lands
//! as attributes, what it reads comes back from them, and the pieces move
//! with their files.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::Instant;

use nfs3_server::nfs3_types::nfs3::{filename3, nfsstat3, sattr3, set_size3, stable_how};
use nfs3_server::vfs::{
    FileHandleU64, NextResult, NfsFileSystem, NfsReadFileSystem, ReadDirPlusIterator,
};

use super::appledouble::{AppleDouble, Attr};
use super::images::{CLEAN_TTL, PENDING_TTL};
use super::side_entry::SIDE_STORE_DIR;
use super::xattrs::Store;
use super::{FLUSH_IDLE, is_sidecar_name};
use crate::machine_export::attr::IdMap;
use crate::machine_export::vfs::MachineRoot;

const ATTRS_ONLY: &[u8] = include_bytes!("testdata/attrs-only.appledouble");
const WITH_FORK: &[u8] = include_bytes!("testdata/with-fork.appledouble");

fn name(s: &str) -> filename3<'_> {
    filename3::from(s.as_bytes())
}

/// An export of `dir` whose guest owner is this user, splitting attributes
/// at 256 bytes so the fixture's 1 336-byte fork takes the side store and
/// its Finder tag stays on the inode.
fn export(dir: &Path) -> MachineRoot {
    let me = std::fs::metadata(dir).unwrap();
    MachineRoot::with_store(
        dir.to_path_buf(),
        IdMap::new(501, 20, me.uid(), me.gid()),
        Store::new(256),
    )
}

async fn listing(fs: &MachineRoot, dir: &FileHandleU64) -> Vec<String> {
    let mut listing = fs.readdirplus(dir, 0).await.unwrap();
    let mut names = Vec::new();
    while let NextResult::Ok(entry) = listing.next().await {
        names.push(String::from_utf8(entry.name.as_ref().to_vec()).unwrap());
    }
    names.sort();
    names
}

fn user_xattrs(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = xattr::list(path)
        .unwrap()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| name.starts_with("user."))
        .collect();
    names.sort();
    names
}

/// The content of a sidecar as read back whole, attributes sorted by name
/// so an image in our layout compares with one in the Mac's.
async fn content(fs: &MachineRoot, sidecar: &FileHandleU64) -> AppleDouble {
    let (bytes, eof) = fs.read(sidecar, 0, 1 << 20).await.unwrap();
    assert!(eof);
    let mut parsed = AppleDouble::parse(&bytes).unwrap();
    parsed.attrs.sort_by(|a, b| a.name.cmp(&b.name));
    parsed
}

fn sorted(image: &[u8]) -> AppleDouble {
    let mut parsed = AppleDouble::parse(image).unwrap();
    parsed.attrs.sort_by(|a, b| a.name.cmp(&b.name));
    parsed
}

#[test]
fn only_a_dot_underscore_prefix_with_a_name_is_a_sidecar() {
    assert!(is_sidecar_name(OsStr::new("._pack.idx")));
    assert!(is_sidecar_name(OsStr::new("._.")), "the directory's own");
    assert!(!is_sidecar_name(OsStr::new("._..")));
    assert!(!is_sidecar_name(OsStr::new("._")));
    assert!(!is_sidecar_name(OsStr::new(".hidden")));
    assert!(!is_sidecar_name(OsStr::new("pack.idx")));
}

#[tokio::test]
async fn what_the_mac_writes_lands_as_the_targets_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f");
    std::fs::write(&f, b"file").unwrap();
    std::fs::write(dir.path().join("._real"), b"machine").unwrap();
    let fs = export(dir.path());
    let root = fs.root_dir();

    // Nothing yet: `f` has no attributes, so it has no sidecar.
    assert_eq!(
        fs.lookup(&root, &name("._f")).await,
        Err(nfsstat3::NFS3ERR_NOENT)
    );

    // The Mac creates one, writes it, commits: the attributes land on `f`.
    let (sc, attr) = fs
        .create(&root, &name("._f"), sattr3::default())
        .await
        .unwrap();
    assert_eq!((attr.size, attr.uid, attr.mode), (0, 501, 0o644));
    assert_eq!(
        fs.create_exclusive(&root, &name("._f"), Default::default())
            .await,
        Err(nfsstat3::NFS3ERR_EXIST)
    );
    let (attr, stable) = fs
        .write(&sc, 0, ATTRS_ONLY, stable_how::UNSTABLE)
        .await
        .unwrap();
    assert_eq!((attr.size, stable), (4096, stable_how::UNSTABLE));
    assert!(
        user_xattrs(&f).is_empty(),
        "nothing lands before the commit"
    );
    fs.commit(&sc, 0, 0).await.unwrap();
    assert_eq!(
        user_xattrs(&f),
        [
            "user.com.apple.metadata:_kMDItemUserTags",
            "user.com.apple.provenance",
            "user.user.note"
        ]
    );
    assert_eq!(xattr::get(&f, "user.user.note").unwrap().unwrap(), b"hi");
    assert!(!dir.path().join("._f").exists());

    // The image the Mac created was merged and dropped: it reads back as
    // XNU would have laid it out, with the same content.
    assert_eq!(fs.lookup(&root, &name("._f")).await.unwrap(), sc);
    let (bytes, eof) = fs.read(&sc, 0, 8192).await.unwrap();
    assert!(eof);
    assert_eq!(bytes.len(), 4096, "a fresh sidecar's size");
    assert_eq!(sorted(&bytes), sorted(ATTRS_ONLY));
    assert_eq!(fs.getattr(&sc).await.unwrap().size, bytes.len() as u64);
    assert_eq!(fs.readlink(&sc).await, Err(nfsstat3::NFS3ERR_INVAL));

    // An attribute set inside the machine shows on the Mac once the read
    // cache retires.
    xattr::set(&f, "user.mime_type", b"text/plain").unwrap();
    fs.retire_sidecars(Instant::now() + CLEAN_TTL).await;
    assert!(
        content(&fs, &sc)
            .await
            .attrs
            .iter()
            .any(|attr| attr.name == b"mime_type")
    );

    // Listed: only what is on disk. A machine-made `._real` is a plain
    // file, and create keeps it.
    assert_eq!(listing(&fs, &root).await, ["._real", "f"]);
    let real = fs.lookup(&root, &name("._real")).await.unwrap();
    assert_eq!(fs.read(&real, 0, 100).await.unwrap().0, b"machine");
    fs.create(&root, &name("._real"), sattr3::default())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("._real")).unwrap(),
        b"machine"
    );

    // A sidecar the Mac creates — believing it new, as after `mv` or with
    // a stale negative cache entry — adds to what the target has.
    let (again, _) = fs
        .create(&root, &name("._f"), sattr3::default())
        .await
        .unwrap();
    assert_eq!(again, sc, "the same handle");
    let added = AppleDouble {
        attrs: vec![Attr {
            name: b"added".to_vec(),
            value: b"1".to_vec(),
        }],
        ..AppleDouble::default()
    };
    fs.write(&sc, 0, &added.to_bytes(), stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&sc, 0, 0).await.unwrap();
    assert_eq!(
        user_xattrs(&f),
        [
            "user.added",
            "user.com.apple.metadata:_kMDItemUserTags",
            "user.com.apple.provenance",
            "user.mime_type",
            "user.user.note"
        ]
    );

    // A sidecar the Mac looked up and rewrote is the whole truth. Nothing
    // lands while the writes may go on; the tick applies them once they
    // pause, and what the image lacks goes.
    let only = AppleDouble {
        attrs: vec![Attr {
            name: b"only".to_vec(),
            value: b"1".to_vec(),
        }],
        ..AppleDouble::default()
    };
    let truncate = sattr3 {
        size: set_size3::Some(0),
        ..sattr3::default()
    };
    assert_eq!(fs.lookup(&root, &name("._f")).await.unwrap(), sc);
    assert_eq!(fs.setattr(&sc, truncate).await.unwrap().size, 0);
    fs.write(&sc, 0, &only.to_bytes(), stable_how::FILE_SYNC)
        .await
        .unwrap();
    assert_eq!(user_xattrs(&f).len(), 5, "not yet");
    fs.retire_sidecars(Instant::now() + FLUSH_IDLE).await;
    assert_eq!(user_xattrs(&f), ["user.only"]);
    assert_eq!(
        fs.read(&sc, 0, 8192).await.unwrap(),
        (only.to_bytes(), true),
        "a replacing image stays as the read cache"
    );

    // Removing the sidecar removes the attributes; its handle goes stale.
    fs.remove(&root, &name("._f")).await.unwrap();
    assert_eq!(user_xattrs(&f), Vec::<String>::new());
    assert!(matches!(
        fs.getattr(&sc).await,
        Err(nfsstat3::NFS3ERR_STALE)
    ));
    assert_eq!(
        fs.remove(&root, &name("._f")).await,
        Err(nfsstat3::NFS3ERR_NOENT)
    );
    assert_eq!(
        fs.lookup(&root, &name("._f")).await,
        Err(nfsstat3::NFS3ERR_NOENT)
    );
}

#[tokio::test]
async fn large_values_take_the_side_store_and_follow_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f");
    std::fs::write(&f, b"file").unwrap();
    let fs = export(dir.path());
    let root = fs.root_dir();

    let (sc, _) = fs
        .create(&root, &name("._f"), sattr3::default())
        .await
        .unwrap();
    fs.write(&sc, 0, WITH_FORK, stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&sc, 0, 0).await.unwrap();
    assert_eq!(
        user_xattrs(&f),
        [
            "user.com.apple.FinderInfo",
            "user.com.apple.metadata:_kMDItemUserTags",
            "user.com.apple.provenance",
            "user.user.note"
        ],
        "the fork is too big for the inode"
    );
    let side = dir.path().join(SIDE_STORE_DIR).join("f");
    assert!(side.exists());

    // The side store is the export's own: hidden, and not to be made.
    assert_eq!(
        fs.lookup(&root, &name(SIDE_STORE_DIR)).await,
        Err(nfsstat3::NFS3ERR_NOENT)
    );
    assert_eq!(listing(&fs, &root).await, ["f"]);
    assert_eq!(
        fs.mkdir(&root, &name(SIDE_STORE_DIR)).await.map(|_| ()),
        Err(nfsstat3::NFS3ERR_ACCES)
    );

    // Read back whole from both places once the image is gone.
    fs.retire_sidecars(Instant::now() + CLEAN_TTL).await;
    assert_eq!(content(&fs, &sc).await, sorted(WITH_FORK));

    // Renamed from the Mac, the file keeps everything, side entry included;
    // the old sidecar is gone, and renaming it after the fact is fine.
    fs.rename(&root, &name("f"), &root, &name("g"))
        .await
        .unwrap();
    assert!(!side.exists());
    assert!(dir.path().join(SIDE_STORE_DIR).join("g").exists());
    assert_eq!(
        fs.lookup(&root, &name("._f")).await,
        Err(nfsstat3::NFS3ERR_NOENT)
    );
    let sg = fs.lookup(&root, &name("._g")).await.unwrap();
    assert_eq!(content(&fs, &sg).await, sorted(WITH_FORK));
    fs.rename(&root, &name("._f"), &root, &name("._g"))
        .await
        .unwrap();

    // Into a subdirectory as well.
    let (d, _) = fs.mkdir(&root, &name("d")).await.unwrap();
    fs.rename(&root, &name("g"), &d, &name("h")).await.unwrap();
    assert!(dir.path().join("d").join(SIDE_STORE_DIR).join("h").exists());
    assert!(!dir.path().join(SIDE_STORE_DIR).exists());
    let sh = fs.lookup(&d, &name("._h")).await.unwrap();
    assert_eq!(content(&fs, &sh).await, sorted(WITH_FORK));

    // To a plain name the sidecar is cross-device; to another sidecar name
    // it moves the attributes from one target to the other.
    assert_eq!(
        fs.rename(&d, &name("._h"), &d, &name("plain")).await,
        Err(nfsstat3::NFS3ERR_XDEV)
    );
    fs.create(&d, &name("other"), sattr3::default())
        .await
        .unwrap();
    fs.rename(&d, &name("._h"), &d, &name("._other"))
        .await
        .unwrap();
    assert_eq!(user_xattrs(&dir.path().join("d/h")), Vec::<String>::new());
    assert!(!dir.path().join("d").join(SIDE_STORE_DIR).join("h").exists());
    assert!(
        dir.path()
            .join("d")
            .join(SIDE_STORE_DIR)
            .join("other")
            .exists()
    );
    let so = fs.lookup(&d, &name("._other")).await.unwrap();
    assert_eq!(content(&fs, &so).await, sorted(WITH_FORK));

    // Removing the file drops its side entry, and a directory left with
    // only its side store is empty to the Mac.
    fs.remove(&d, &name("other")).await.unwrap();
    assert!(!dir.path().join("d").join(SIDE_STORE_DIR).exists());
    fs.remove(&d, &name("h")).await.unwrap();
    std::fs::create_dir_all(dir.path().join("d").join(SIDE_STORE_DIR)).unwrap();
    std::fs::write(
        dir.path().join("d").join(SIDE_STORE_DIR).join("gone"),
        b"stale",
    )
    .unwrap();
    assert_eq!(listing(&fs, &d).await, Vec::<String>::new());
    fs.remove(&root, &name("d")).await.unwrap();
    assert!(!dir.path().join("d").exists());
}

#[tokio::test]
async fn a_sidecar_written_ahead_of_its_file_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let fs = export(dir.path());
    let root = fs.root_dir();

    // `cp -R` of a volume with real `._` files: the sidecar comes first.
    let (sc, attr) = fs
        .create(&root, &name("._later"), sattr3::default())
        .await
        .unwrap();
    assert_eq!(
        (attr.uid, attr.mode),
        (501, 0o644),
        "no target: the guest owner's"
    );
    fs.write(&sc, 0, ATTRS_ONLY, stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&sc, 0, 0).await.unwrap();
    assert!(!dir.path().join("later").exists());
    assert_eq!(fs.lookup(&root, &name("._later")).await.unwrap(), sc);
    fs.create(&root, &name("later"), sattr3::default())
        .await
        .unwrap();
    assert_eq!(
        xattr::get(dir.path().join("later"), "user.user.note")
            .unwrap()
            .unwrap(),
        b"hi"
    );

    // Through a rename into place as well.
    let (sm, _) = fs
        .create(&root, &name("._moved"), sattr3::default())
        .await
        .unwrap();
    fs.write(&sm, 0, ATTRS_ONLY, stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&sm, 0, 0).await.unwrap();
    fs.create(&root, &name("tmp"), sattr3::default())
        .await
        .unwrap();
    fs.rename(&root, &name("tmp"), &root, &name("moved"))
        .await
        .unwrap();
    assert_eq!(
        xattr::get(dir.path().join("moved"), "user.user.note")
            .unwrap()
            .unwrap(),
        b"hi"
    );

    // `._.` is the directory's own.
    let (e, _) = fs.mkdir(&root, &name("e")).await.unwrap();
    let (se, attr) = fs
        .create(&e, &name("._."), sattr3::default())
        .await
        .unwrap();
    assert_eq!(attr.mode, 0o644, "the directory's read and write bits");
    fs.write(&se, 0, ATTRS_ONLY, stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&se, 0, 0).await.unwrap();
    assert_eq!(
        xattr::get(dir.path().join("e"), "user.user.note")
            .unwrap()
            .unwrap(),
        b"hi"
    );
    assert_eq!(listing(&fs, &e).await, Vec::<String>::new());

    // A file that merely has a `._` name lands on disk when committed, and
    // is a plain file from then on.
    let (sp, _) = fs
        .create(&root, &name("._plain"), sattr3::default())
        .await
        .unwrap();
    fs.write(&sp, 0, b"just text\n", stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&sp, 0, 0).await.unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("._plain")).unwrap(),
        b"just text\n"
    );
    assert_eq!(fs.lookup(&root, &name("._plain")).await.unwrap(), sp);
    assert_eq!(
        fs.read(&sp, 0, 100).await.unwrap(),
        (b"just text\n".to_vec(), true)
    );
    assert!(listing(&fs, &root).await.contains(&"._plain".to_owned()));

    // One the Mac never finishes is dropped when it expires.
    let (sg, _) = fs
        .create(&root, &name("._gone"), sattr3::default())
        .await
        .unwrap();
    fs.write(&sg, 0, &ATTRS_ONLY[..100], stable_how::UNSTABLE)
        .await
        .unwrap();
    fs.commit(&sg, 0, 0).await.unwrap();
    fs.retire_sidecars(Instant::now() + PENDING_TTL).await;
    assert_eq!(
        fs.lookup(&root, &name("._gone")).await,
        Err(nfsstat3::NFS3ERR_NOENT)
    );
    assert!(!dir.path().join("._gone").exists());
}
