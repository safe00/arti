//! Net document storage backed by sqlite3.
//!
//! We store most objects in sqlite tables, except for very large ones,
//! which we store as "blob" files in a separate directory.

use crate::docmeta::{AuthCertMeta, ConsensusMeta};
use crate::storage::InputString;
use crate::{Error, Result};

use tor_netdoc::doc::authcert::AuthCertKeyIds;
use tor_netdoc::doc::microdesc::MdDigest;
use tor_netdoc::doc::netstatus::{ConsensusFlavor, Lifetime};
use tor_netdoc::doc::routerdesc::RdDigest;

use std::collections::HashMap;
use std::convert::TryInto;
use std::path::{self, Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;
use chrono::prelude::*;
use chrono::Duration as CDuration;
use rusqlite::{params, OpenFlags, OptionalExtension, Transaction};

#[cfg(target_family = "unix")]
use std::os::unix::fs::DirBuilderExt;

/// Local directory cache using a Sqlite3 connection.
pub(crate) struct SqliteStore {
    /// Connection to the sqlite3 database.
    conn: rusqlite::Connection,
    /// Location for the sqlite3 database; used to reopen it.
    sql_path: Option<PathBuf>,
    /// Location to store blob files.
    path: PathBuf,
    /// Lockfile to prevent concurrent write attempts from different
    /// processes.
    ///
    /// If this is None we aren't using a lockfile.  Watch out!
    ///
    /// (sqlite supports that with connection locking, but we want to
    /// be a little more coarse-grained here)
    // XXXX This can behave oddly or fail if this process already has
    // XXXX another instance of this file; see fslock documentation.
    lockfile: Option<fslock::LockFile>,
}

impl SqliteStore {
    /// Construct or open a new SqliteStore at some location on disk.
    /// The provided location must be a directory, or a possible
    /// location for a directory: the directory will be created if
    /// necessary.
    ///
    /// If readonly is true, the result will be a read-only store.
    /// Otherwise, when readonly is false, the result may be
    /// read-only or read-write, depending on whether we can acquire
    /// the lock.
    ///
    /// # Limitations:
    ///
    /// The file locking that we use to ensure that only one dirmgr is
    /// writing to a given storage directory at a time is currently
    /// _per process_. Therefore, you might get unexpected results if
    /// two SqliteStores are created in the same process with the
    /// path.
    pub(crate) fn from_path<P: AsRef<Path>>(path: P, mut readonly: bool) -> Result<Self> {
        let path = path.as_ref();
        let sqlpath = path.join("dir.sqlite3");
        let blobpath = path.join("dir_blobs/");
        let lockpath = path.join("dir.lock");

        #[cfg(target_family = "unix")]
        if !readonly {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&blobpath)
                .with_context(|| format!("Creating directory at {:?}", &blobpath))?;
        }
        #[cfg(not(target_family = "unix"))]
        if !readonly {
            std::fs::DirBuilder::new()
                .recursive(true)
                .create(&blobpath)
                .with_context(|| format!("Creating directory at {:?}", &blobpath))?;
        }

        let mut lockfile = fslock::LockFile::open(&lockpath)?;
        if !readonly && !lockfile.try_lock()? {
            readonly = true; // we couldn't get the lock!
        };
        let flags = if readonly {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        };
        let conn = rusqlite::Connection::open_with_flags(&sqlpath, flags)?;
        let mut store = SqliteStore::from_conn(conn, &blobpath)?;
        store.sql_path = Some(sqlpath);
        store.lockfile = Some(lockfile);
        Ok(store)
    }

    /// Construct a new SqliteStore from a database connection and a location
    /// for blob files.
    ///
    /// Used for testing with a memory-backed database.
    pub(crate) fn from_conn<P>(conn: rusqlite::Connection, path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_path_buf();
        let mut result = SqliteStore {
            conn,
            path,
            lockfile: None,
            sql_path: None,
        };

        result.check_schema()?;

        Ok(result)
    }

    /// Return true if this store is opened in read-only mode.
    pub(crate) fn is_readonly(&self) -> bool {
        match &self.lockfile {
            Some(f) => !f.owns_lock(),
            None => false,
        }
    }

    /// Try to upgrade from a read-only connection to a read-write connection.
    ///
    /// Return true on succcess; false if another process had the lock.
    pub(crate) fn upgrade_to_readwrite(&mut self) -> Result<bool> {
        if self.is_readonly() && self.sql_path.is_some() {
            let lf = self.lockfile.as_mut().unwrap();
            if !lf.try_lock()? {
                // Somebody else has the lock.
                return Ok(false);
            }
            match rusqlite::Connection::open(self.sql_path.as_ref().unwrap()) {
                Ok(conn) => {
                    self.conn = conn;
                }
                Err(e) => {
                    let _ignore = lf.unlock();
                    return Err(e.into());
                }
            }
        }
        Ok(true)
    }

    /// Check whether this database has a schema format we can read, and
    /// install or upgrade the schema if necessary.
    fn check_schema(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let db_n_tables: u32 = tx.query_row(
            "SELECT COUNT(name) FROM sqlite_master
             WHERE type='table'
             AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        let db_exists = db_n_tables > 0;

        if !db_exists {
            tx.execute_batch(INSTALL_V0_SCHEMA)?;
            tx.execute_batch(UPDATE_SCHEMA_V0_TO_V1)?;
            tx.commit()?;
            return Ok(());
        }

        let (version, readable_by): (u32, u32) = tx.query_row(
            "SELECT version, readable_by FROM TorSchemaMeta
             WHERE name = 'TorDirStorage'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        if version < SCHEMA_VERSION {
            // Update the schema.
            tx.execute_batch(UPDATE_SCHEMA_V0_TO_V1)?;
            tx.commit()?;
            return Ok(());
        } else if readable_by > SCHEMA_VERSION {
            return Err(Error::UnrecognizedSchema.into());
        }

        // rolls back the transaction, but nothing was done.
        Ok(())
    }

    /// Delete all completely-expired objects from the database.
    ///
    /// This is pretty conservative, and only removes things that are
    /// definitely past their good-by date.
    pub(crate) fn expire_all(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let expired_blobs: Vec<String> = {
            let mut stmt = tx.prepare(FIND_EXPIRED_EXTDOCS)?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .filter_map(std::result::Result::ok)
                .collect();
            names
        };

        tx.execute(DROP_OLD_EXTDOCS, [])?;
        tx.execute(DROP_OLD_MICRODESCS, [])?;
        tx.execute(DROP_OLD_AUTHCERTS, [])?;
        tx.execute(DROP_OLD_CONSENSUSES, [])?;
        tx.execute(DROP_OLD_ROUTERDESCS, [])?;
        tx.commit()?;
        for name in expired_blobs {
            let fname = self.blob_fname(name);
            if let Ok(fname) = fname {
                let _ignore = std::fs::remove_file(fname);
            }
        }
        Ok(())
    }

    /// Return the correct filename for a given blob, based on the filename
    /// from the ExtDocs table.
    fn blob_fname<P>(&self, path: P) -> Result<PathBuf>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        if !path
            .components()
            .all(|c| matches!(c, path::Component::Normal(_)))
        {
            return Err(Error::CacheCorruption("Invalid path in database").into());
        }

        let mut result = self.path.clone();
        result.push(path);
        Ok(result)
    }

    /// Read a blob from disk, mmapping it if possible.
    fn read_blob<P>(&self, path: P) -> Result<InputString>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let full_path = self.blob_fname(path)?;
        InputString::load(&full_path)
            .with_context(|| format!("Loading blob {:?} from storage at {:?}", path, full_path))
    }

    /// Write a file to disk as a blob, and record it in the ExtDocs table.
    ///
    /// Return a SavedBlobHandle that describes where the blob is, and which
    /// can be used either to commit the blob or delete it.
    fn save_blob_internal(
        &mut self,
        contents: &[u8],
        doctype: &str,
        dtype: &str,
        digest: &[u8],
        expires: DateTime<Utc>,
    ) -> Result<SavedBlobHandle<'_>> {
        let digest = hex::encode(digest);
        let digeststr = format!("{}-{}", dtype, digest);
        let fname = format!("{}:{}", doctype, digeststr);
        let full_path = self.blob_fname(&fname)?;

        let unlinker = Unlinker::new(&full_path);
        std::fs::write(full_path, contents)?;

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(INSERT_EXTDOC, params![digeststr, expires, dtype, fname])?;

        Ok(SavedBlobHandle {
            tx,
            fname,
            digeststr,
            unlinker,
        })
    }

    /// Save a blob to disk and commit it.
    #[cfg(test)]
    fn save_blob(
        &mut self,
        contents: &[u8],
        doctype: &str,
        dtype: &str,
        digest: &[u8],
        expires: DateTime<Utc>,
    ) -> Result<String> {
        let h = self.save_blob_internal(contents, doctype, dtype, digest, expires)?;
        let SavedBlobHandle {
            tx,
            digeststr,
            fname,
            unlinker,
        } = h;
        let _ = digeststr;
        tx.commit()?;
        unlinker.forget();
        Ok(fname)
    }

    /// Write a consensus to disk.
    pub(crate) fn store_consensus(
        &mut self,
        cmeta: &ConsensusMeta,
        flavor: ConsensusFlavor,
        pending: bool,
        contents: &str,
    ) -> Result<()> {
        let lifetime = cmeta.lifetime();
        let sha3_of_signed = cmeta.sha3_256_of_signed();
        let sha3_of_whole = cmeta.sha3_256_of_whole();
        let valid_after: DateTime<Utc> = lifetime.valid_after().into();
        let fresh_until: DateTime<Utc> = lifetime.fresh_until().into();
        let valid_until: DateTime<Utc> = lifetime.valid_until().into();

        // After a few days have passed, a consensus is no good for
        // anything at all, not even diffs.
        let expires = valid_until + CDuration::days(4);

        let doctype = format!("con:{}", flavor.name());

        let h = self.save_blob_internal(
            contents.as_bytes(),
            &doctype,
            "sha3-256",
            &sha3_of_whole[..],
            expires,
        )?;
        h.tx.execute(
            INSERT_CONSENSUS,
            params![
                valid_after,
                fresh_until,
                valid_until,
                flavor.name(),
                pending,
                hex::encode(&sha3_of_signed),
                h.digeststr
            ],
        )?;
        h.tx.commit()?;
        h.unlinker.forget();
        Ok(())
    }

    /// Return the information about the latest non-pending consensus,
    /// including its valid-after time and digest.
    pub(crate) fn latest_consensus_meta(
        &self,
        flavor: ConsensusFlavor,
    ) -> Result<Option<ConsensusMeta>> {
        let mut stmt = self.conn.prepare(FIND_LATEST_CONSENSUS_META)?;
        let mut rows = stmt.query(params![flavor.name()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(cmeta_from_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Return the valid-after time for the latest non non-pending consensus,
    #[cfg(test)]
    // We should revise the tests to use latest_consensus_meta instead.
    fn latest_consensus_time(&self, flavor: ConsensusFlavor) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .latest_consensus_meta(flavor)?
            .map(|m| m.lifetime().valid_after().into()))
    }

    /// Load the latest consensus from disk.
    ///
    /// If `pending` is given, we will only return a consensus with
    /// the given "pending" status.  (A pending consensus doesn't have
    /// enough descriptors yet.)  If `pending_ok` is None, we'll
    /// return a consensus with any pending status.
    pub(crate) fn latest_consensus(
        &self,
        flavor: ConsensusFlavor,
        pending: Option<bool>,
    ) -> Result<Option<InputString>> {
        let rv: Option<(DateTime<Utc>, DateTime<Utc>, String)>;
        rv = match pending {
            None => self
                .conn
                .query_row(FIND_CONSENSUS, params![flavor.name()], |row| row.try_into())
                .optional()?,
            Some(pending_val) => self
                .conn
                .query_row(
                    FIND_CONSENSUS_P,
                    params![pending_val, flavor.name()],
                    |row| row.try_into(),
                )
                .optional()?,
        };

        if let Some((_va, _vu, filename)) = rv {
            self.read_blob(filename).map(Option::Some)
        } else {
            Ok(None)
        }
    }

    /// Try to read the consensus corresponding to the provided metadata object.
    #[allow(unused)]
    pub(crate) fn consensus_by_meta(&self, cmeta: &ConsensusMeta) -> Result<InputString> {
        if let Some((text, _)) =
            self.consensus_by_sha3_digest_of_signed_part(cmeta.sha3_256_of_signed())?
        {
            Ok(text)
        } else {
            Err(Error::CacheCorruption("couldn't find a consensus we thought we had.").into())
        }
    }

    /// Try to read the consensus whose SHA3-256 digests is the provided
    /// value, and its metadata.
    pub(crate) fn consensus_by_sha3_digest_of_signed_part(
        &self,
        d: &[u8; 32],
    ) -> Result<Option<(InputString, ConsensusMeta)>> {
        let digest = hex::encode(d);
        let mut stmt = self
            .conn
            .prepare(FIND_CONSENSUS_AND_META_BY_DIGEST_OF_SIGNED)?;
        let mut rows = stmt.query(params![digest])?;
        if let Some(row) = rows.next()? {
            let meta = cmeta_from_row(row)?;
            let fname: String = row.get(5)?;
            let text = self.read_blob(&fname)?;
            Ok(Some((text, meta)))
        } else {
            Ok(None)
        }
    }

    /// Mark the consensus generated from `cmeta` as no longer pending.
    pub(crate) fn mark_consensus_usable(&mut self, cmeta: &ConsensusMeta) -> Result<()> {
        let d = hex::encode(cmeta.sha3_256_of_whole());
        let digest = format!("sha3-256-{}", d);

        let tx = self.conn.transaction()?;
        tx.execute(MARK_CONSENSUS_NON_PENDING, params![digest])?;
        tx.commit()?;

        Ok(())
    }

    /// Remove the consensus generated from `cmeta`.
    #[allow(unused)]
    pub(crate) fn delete_consensus(&mut self, cmeta: &ConsensusMeta) -> Result<()> {
        let d = hex::encode(cmeta.sha3_256_of_whole());
        let digest = format!("sha3-256-{}", d);

        // TODO: We should probably remove the blob as well, but for now
        // this is enough.
        let tx = self.conn.transaction()?;
        tx.execute(REMOVE_CONSENSUS, params![digest])?;
        tx.commit()?;

        Ok(())
    }

    /// Save a list of authority certificates to the cache.
    pub(crate) fn store_authcerts(&mut self, certs: &[(AuthCertMeta, &str)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(INSERT_AUTHCERT)?;
        for (meta, content) in certs {
            let ids = meta.key_ids();
            let id_digest = hex::encode(ids.id_fingerprint.as_bytes());
            let sk_digest = hex::encode(ids.sk_fingerprint.as_bytes());
            let published: DateTime<Utc> = meta.published().into();
            let expires: DateTime<Utc> = meta.expires().into();
            stmt.execute(params![id_digest, sk_digest, published, expires, content])?;
        }
        stmt.finalize()?;
        tx.commit()?;
        Ok(())
    }

    /// Read all of the specified authority certs from the cache.
    pub(crate) fn authcerts(
        &self,
        certs: &[AuthCertKeyIds],
    ) -> Result<HashMap<AuthCertKeyIds, String>> {
        let mut result = HashMap::new();
        // XXXX Do I need to get a transaction here for performance?
        let mut stmt = self.conn.prepare(FIND_AUTHCERT)?;

        for ids in certs {
            let id_digest = hex::encode(ids.id_fingerprint.as_bytes());
            let sk_digest = hex::encode(ids.sk_fingerprint.as_bytes());
            if let Some(contents) = stmt
                .query_row(params![id_digest, sk_digest], |row| row.get::<_, String>(0))
                .optional()?
            {
                result.insert(*ids, contents);
            }
        }

        Ok(result)
    }

    /// Read all the microdescriptors listed in `input` from the cache.
    pub(crate) fn microdescs<'a, I>(&self, input: I) -> Result<HashMap<MdDigest, String>>
    where
        I: IntoIterator<Item = &'a MdDigest>,
    {
        let mut result = HashMap::new();
        let mut stmt = self.conn.prepare(FIND_MD)?;

        // XXXX Should I speed this up with a transaction, or does it not
        // matter for queries?
        for md_digest in input.into_iter() {
            let h_digest = hex::encode(md_digest);
            if let Some(contents) = stmt
                .query_row(params![h_digest], |row| row.get::<_, String>(0))
                .optional()?
            {
                result.insert(*md_digest, contents);
            }
        }

        Ok(result)
    }

    /// Read all the microdescriptors listed in `input` from the cache.
    pub(crate) fn routerdescs<'a, I>(&self, input: I) -> Result<HashMap<RdDigest, String>>
    where
        I: IntoIterator<Item = &'a RdDigest>,
    {
        let mut result = HashMap::new();
        let mut stmt = self.conn.prepare(FIND_RD)?;

        // XXXX Should I speed this up with a transaction, or does it not
        // matter for queries?
        for rd_digest in input.into_iter() {
            let h_digest = hex::encode(rd_digest);
            if let Some(contents) = stmt
                .query_row(params![h_digest], |row| row.get::<_, String>(0))
                .optional()?
            {
                result.insert(*rd_digest, contents);
            }
        }

        Ok(result)
    }

    /// Update the `last-listed` time of every microdescriptor in
    /// `input` to `when` or later.
    pub(crate) fn update_microdescs_listed<'a, I>(
        &mut self,
        input: I,
        when: SystemTime,
    ) -> Result<()>
    where
        I: IntoIterator<Item = &'a MdDigest>,
    {
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(UPDATE_MD_LISTED)?;
        let when: DateTime<Utc> = when.into();

        for md_digest in input.into_iter() {
            let h_digest = hex::encode(md_digest);
            stmt.execute(params![when, h_digest])?;
        }

        stmt.finalize()?;
        tx.commit()?;
        Ok(())
    }

    /// Store every microdescriptor in `input` into the cache, and say that
    /// it was last listed at `when`.
    pub(crate) fn store_microdescs<'a, I>(&mut self, input: I, when: SystemTime) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, &'a MdDigest)>,
    {
        let when: DateTime<Utc> = when.into();

        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(INSERT_MD)?;

        for (content, md_digest) in input.into_iter() {
            let h_digest = hex::encode(md_digest);
            stmt.execute(params![h_digest, when, content])?;
        }
        stmt.finalize()?;
        tx.commit()?;
        Ok(())
    }

    /// Store every router descriptors in `input` into the cache.
    #[allow(unused)]
    pub(crate) fn store_routerdescs<'a, I>(&mut self, input: I) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, SystemTime, &'a RdDigest)>,
    {
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare(INSERT_RD)?;

        for (content, when, rd_digest) in input.into_iter() {
            let when: DateTime<Utc> = when.into();
            let h_digest = hex::encode(rd_digest);
            stmt.execute(params![h_digest, when, content])?;
        }
        stmt.finalize()?;
        tx.commit()?;
        Ok(())
    }
}

/// Handle to a blob that we have saved to disk but not yet committed to
/// the database.
struct SavedBlobHandle<'a> {
    /// Transaction we're using to add the blob to the ExtDocs table.
    tx: Transaction<'a>,
    /// Filename for the file, with respect to the the blob directory.
    #[allow(unused)]
    fname: String,
    /// Declared digest string for this blob. Of the format
    /// "digesttype-hexstr".
    digeststr: String,
    /// An 'unlinker' for the blob file.
    unlinker: Unlinker,
}

/// Handle to a file which we might have to delete.
///
/// When this handle is dropped, the file gets deleted, unless you have
/// first called [`Unlinker::forget`].
struct Unlinker {
    /// The location of the file to remove, or None if we shouldn't
    /// remove it.
    p: Option<PathBuf>,
}
impl Unlinker {
    /// Make a new Unlinker for a given filename.
    fn new<P: AsRef<Path>>(p: P) -> Self {
        Unlinker {
            p: Some(p.as_ref().to_path_buf()),
        }
    }
    /// Forget about this unlinker, so that the corresponding file won't
    /// get dropped.
    fn forget(mut self) {
        self.p = None
    }
}
impl Drop for Unlinker {
    fn drop(&mut self) {
        if let Some(p) = self.p.take() {
            let _ignore_err = std::fs::remove_file(p);
        }
    }
}

/// Convert a hexadecimal sha3-256 digest from the database into an array.
fn digest_from_hex(s: &str) -> Result<[u8; 32]> {
    hex::decode(s)?
        .try_into()
        .map_err(|_| Error::CacheCorruption("Invalid digest in database").into())
}

/// Convert a hexadecimal sha3-256 "digest string" as used in the
/// digest column from the database into an array.
fn digest_from_dstr(s: &str) -> Result<[u8; 32]> {
    if let Some(stripped) = s.strip_prefix("sha3-256-") {
        hex::decode(stripped)?
            .try_into()
            .map_err(|_| Error::CacheCorruption("Invalid digest in database").into())
    } else {
        Err(Error::CacheCorruption("Invalid digest in database").into())
    }
}

/// Create a ConsensusMeta from a `Row` returned by one of
/// `FIND_LATEST_CONSENSUS_META` or `FIND_CONSENSUS_AND_META_BY_DIGEST`.
fn cmeta_from_row(row: &rusqlite::Row<'_>) -> Result<ConsensusMeta> {
    let va: DateTime<Utc> = row.get(0)?;
    let fu: DateTime<Utc> = row.get(1)?;
    let vu: DateTime<Utc> = row.get(2)?;
    let d_signed: String = row.get(3)?;
    let d_all: String = row.get(4)?;
    let lifetime = Lifetime::new(va.into(), fu.into(), vu.into())?;
    Ok(ConsensusMeta::new(
        lifetime,
        digest_from_hex(&d_signed)?,
        digest_from_dstr(&d_all)?,
    ))
}

/// Version number used for this version of the arti cache schema.
const SCHEMA_VERSION: u32 = 1;

/// Set up the tables for the arti cache schema in a sqlite database.
const INSTALL_V0_SCHEMA: &str = "
  -- Helps us version the schema.  The schema here corresponds to a
  -- version number called 'version', and it should be readable by
  -- anybody who is compliant with versions of at least 'readable_by'.
  CREATE TABLE TorSchemaMeta (
     name TEXT NOT NULL PRIMARY KEY,
     version INTEGER NOT NULL,
     readable_by INTEGER NOT NULL
  );

  INSERT INTO TorSchemaMeta (name, version, readable_by) VALUES ( 'TorDirStorage', 0, 0 );

  -- Keeps track of external blobs on disk.
  CREATE TABLE ExtDocs (
    -- Records a digest of the file contents, in the form 'dtype-hexstr'
    digest TEXT PRIMARY KEY NOT NULL,
    -- When was this file created?
    created DATE NOT NULL,
    -- After what time will this file definitely be useless?
    expires DATE NOT NULL,
    -- What is the type of this file? Currently supported are 'con:<flavor>'.
    type TEXT NOT NULL,
    -- Filename for this file within our blob directory.
    filename TEXT NOT NULL
  );

  -- All the microdescriptors we know about.
  CREATE TABLE Microdescs (
    sha256_digest TEXT PRIMARY KEY NOT NULL,
    last_listed DATE NOT NULL,
    contents BLOB NOT NULL
  );

  -- All the authority certificates we know.
  CREATE TABLE Authcerts (
    id_digest TEXT NOT NULL,
    sk_digest TEXT NOT NULL,
    published DATE NOT NULL,
    expires DATE NOT NULL,
    contents BLOB NOT NULL,
    PRIMARY KEY (id_digest, sk_digest)
  );

  -- All the consensuses we're storing.
  CREATE TABLE Consensuses (
    valid_after DATE NOT NULL,
    fresh_until DATE NOT NULL,
    valid_until DATE NOT NULL,
    flavor TEXT NOT NULL,
    pending BOOLEAN NOT NULL,
    sha3_of_signed_part TEXT NOT NULL,
    digest TEXT NOT NULL,
    FOREIGN KEY (digest) REFERENCES ExtDocs (digest) ON DELETE CASCADE
  );
  CREATE INDEX Consensuses_vu on CONSENSUSES(valid_until);

";

/// Update the database schema from version 0 to version 1.
const UPDATE_SCHEMA_V0_TO_V1: &str = "
  CREATE TABLE RouterDescs (
    sha1_digest TEXT PRIMARY KEY NOT NULL,
    published DATE NOT NULL,
    contents BLOB NOT NULL
  );

  UPDATE TorSchemaMeta SET version=1 WHERE version<1;
";

/// Query: find the latest-expiring microdesc consensus with a given
/// pending status.
const FIND_CONSENSUS_P: &str = "
  SELECT valid_after, valid_until, filename
  FROM Consensuses
  INNER JOIN ExtDocs ON ExtDocs.digest = Consensuses.digest
  WHERE pending = ? AND flavor = ?
  ORDER BY valid_until DESC
  LIMIT 1;
";

/// Query: find the latest-expiring microdesc consensus, regardless of
/// pending status.
const FIND_CONSENSUS: &str = "
  SELECT valid_after, valid_until, filename
  FROM Consensuses
  INNER JOIN ExtDocs ON ExtDocs.digest = Consensuses.digest
  WHERE flavor = ?
  ORDER BY valid_until DESC
  LIMIT 1;
";

/// Query: Find the valid-after time for the latest-expiring
/// non-pending consensus of a given flavor.
const FIND_LATEST_CONSENSUS_META: &str = "
  SELECT valid_after, fresh_until, valid_until, sha3_of_signed_part, digest
  FROM Consensuses
  WHERE pending = 0 AND flavor = ?
  ORDER BY valid_until DESC
  LIMIT 1;
";

/// Look up a consensus by its digest-of-signed-part string.
const FIND_CONSENSUS_AND_META_BY_DIGEST_OF_SIGNED: &str = "
  SELECT valid_after, fresh_until, valid_until, sha3_of_signed_part, Consensuses.digest, filename
  FROM Consensuses
  INNER JOIN ExtDocs on ExtDocs.digest = Consensuses.digest
  WHERE Consensuses.sha3_of_signed_part = ?
  LIMIT 1;
";

/// Query: Update the consensus whose digest field is 'digest' to call it
/// no longer pending.
const MARK_CONSENSUS_NON_PENDING: &str = "
  UPDATE Consensuses
  SET pending = 0
  WHERE digest = ?;
";

/// Query: Remove the consensus with a given digest field.
const REMOVE_CONSENSUS: &str = "
  DELETE FROM Consensuses
  WHERE digest = ?;
";

/// Query: Find the authority certificate with given key digests.
const FIND_AUTHCERT: &str = "
  SELECT contents FROM AuthCerts WHERE id_digest = ? AND sk_digest = ?;
";

/// Query: find the microdescriptor with a given hex-encoded sha256 digest
const FIND_MD: &str = "
  SELECT contents
  FROM Microdescs
  WHERE sha256_digest = ?
";

/// Query: find the router descriptors with a given hex-encoded sha1 digest
const FIND_RD: &str = "
  SELECT contents
  FROM RouterDescs
  WHERE sha1_digest = ?
";

/// Query: find every ExtDocs member that has expired.
const FIND_EXPIRED_EXTDOCS: &str = "
  SELECT filename FROM Extdocs where expires < datetime('now');
";

/// Query: Add a new entry to ExtDocs.
const INSERT_EXTDOC: &str = "
  INSERT OR REPLACE INTO ExtDocs ( digest, created, expires, type, filename )
  VALUES ( ?, datetime('now'), ?, ?, ? );
";

/// Qury: Add a new consensus.
const INSERT_CONSENSUS: &str = "
  INSERT OR REPLACE INTO Consensuses
    ( valid_after, fresh_until, valid_until, flavor, pending, sha3_of_signed_part, digest )
  VALUES ( ?, ?, ?, ?, ?, ?, ? );
";

/// Query: Add a new AuthCert
const INSERT_AUTHCERT: &str = "
  INSERT OR REPLACE INTO Authcerts
    ( id_digest, sk_digest, published, expires, contents)
  VALUES ( ?, ?, ?, ?, ? );
";

/// Query: Add a new microdescriptor
const INSERT_MD: &str = "
  INSERT OR REPLACE INTO Microdescs ( sha256_digest, last_listed, contents )
  VALUES ( ?, ?, ? );
";

/// Query: Add a new router descriptor
#[allow(unused)]
const INSERT_RD: &str = "
  INSERT OR REPLACE INTO RouterDescs ( sha1_digest, published, contents )
  VALUES ( ?, ?, ? );
";

/// Query: Change the time when a given microdescriptor was last listed.
const UPDATE_MD_LISTED: &str = "
  UPDATE Microdescs
  SET last_listed = max(last_listed, ?)
  WHERE sha256_digest = ?;
";

/// Query: Discard every expired extdoc.
const DROP_OLD_EXTDOCS: &str = "
  DELETE FROM ExtDocs WHERE expires < datetime('now');
";
/// Query: Discard every router descriptor that hasn't been listed for 3
/// months.
// TODO: Choose a more realistic time.
const DROP_OLD_ROUTERDESCS: &str = "
  DELETE FROM RouterDescs WHERE published < datetime('now','-3 months');
  ";
/// Query: Discard every microdescriptor that hasn't been listed for 3 months.
// TODO: Choose a more realistic time.
const DROP_OLD_MICRODESCS: &str = "
  DELETE FROM Microdescs WHERE last_listed < datetime('now','-3 months');
";
/// Query: Discard every expired authority certificate.
const DROP_OLD_AUTHCERTS: &str = "
  DELETE FROM Authcerts WHERE expires < datetime('now');
";
/// Query: Discard every consensus that's been expired for at least
/// two days.
const DROP_OLD_CONSENSUSES: &str = "
  DELETE FROM Consensuses WHERE valid_until < datetime('now','-2 days');
";

#[cfg(test)]
mod test {
    use super::*;
    use hex_literal::hex;
    use tempdir::TempDir;

    fn new_empty() -> Result<(TempDir, SqliteStore)> {
        let tmp_dir = TempDir::new("arti-nd").unwrap();
        let sql_path = tmp_dir.path().join("db.sql");
        let conn = rusqlite::Connection::open(&sql_path)?;
        let store = SqliteStore::from_conn(conn, &tmp_dir)?;

        Ok((tmp_dir, store))
    }

    #[test]
    fn init() -> Result<()> {
        let tmp_dir = TempDir::new("arti-nd").unwrap();
        let sql_path = tmp_dir.path().join("db.sql");
        // Initial setup: everything should work.
        {
            let conn = rusqlite::Connection::open(&sql_path)?;
            let _store = SqliteStore::from_conn(conn, &tmp_dir)?;
        }
        // Second setup: shouldn't need to upgrade.
        {
            let conn = rusqlite::Connection::open(&sql_path)?;
            let _store = SqliteStore::from_conn(conn, &tmp_dir)?;
        }
        // Third setup: shouldn't need to upgrade.
        {
            let conn = rusqlite::Connection::open(&sql_path)?;
            conn.execute_batch("UPDATE TorSchemaMeta SET version = 9002;")?;
            let _store = SqliteStore::from_conn(conn, &tmp_dir)?;
        }
        // Fourth: this says we can't read it, so we'll get an error.
        {
            let conn = rusqlite::Connection::open(&sql_path)?;
            conn.execute_batch("UPDATE TorSchemaMeta SET readable_by = 9001;")?;
            let val = SqliteStore::from_conn(conn, &tmp_dir);
            assert!(val.is_err());
        }
        Ok(())
    }

    #[test]
    fn bad_blob_fnames() -> Result<()> {
        let (_tmp_dir, store) = new_empty()?;

        assert!(store.blob_fname("abcd").is_ok());
        assert!(store.blob_fname("abcd..").is_ok());
        assert!(store.blob_fname("..abcd..").is_ok());
        assert!(store.blob_fname(".abcd").is_ok());

        assert!(store.blob_fname(".").is_err());
        assert!(store.blob_fname("..").is_err());
        assert!(store.blob_fname("../abcd").is_err());
        assert!(store.blob_fname("/abcd").is_err());

        Ok(())
    }

    #[test]
    fn blobs() -> Result<()> {
        let (tmp_dir, mut store) = new_empty()?;

        let now = Utc::now();
        let one_week = CDuration::weeks(1);

        let fname1 = store.save_blob(
            b"Hello world",
            "greeting",
            "sha1",
            &hex!("7b502c3a1f48c8609ae212cdfb639dee39673f5e"),
            now + one_week,
        )?;

        let fname2 = store.save_blob(
            b"Goodbye, dear friends",
            "greeting",
            "sha1",
            &hex!("2149c2a7dbf5be2bb36fb3c5080d0fb14cb3355c"),
            now - one_week,
        )?;

        assert_eq!(
            fname1,
            "greeting:sha1-7b502c3a1f48c8609ae212cdfb639dee39673f5e"
        );
        assert_eq!(store.blob_fname(&fname1)?, tmp_dir.path().join(&fname1));
        assert_eq!(
            &std::fs::read(store.blob_fname(&fname1)?)?[..],
            b"Hello world"
        );
        assert_eq!(
            &std::fs::read(store.blob_fname(&fname2)?)?[..],
            b"Goodbye, dear friends"
        );

        let n: u32 = store
            .conn
            .query_row("SELECT COUNT(filename) FROM ExtDocs", [], |row| row.get(0))?;
        assert_eq!(n, 2);

        let blob = store.read_blob(&fname2)?;
        assert_eq!(blob.as_str().unwrap(), "Goodbye, dear friends");

        // Now expire: the second file should go away.
        store.expire_all()?;
        assert_eq!(
            &std::fs::read(store.blob_fname(&fname1)?)?[..],
            b"Hello world"
        );
        assert!(std::fs::read(store.blob_fname(&fname2)?).is_err());
        let n: u32 = store
            .conn
            .query_row("SELECT COUNT(filename) FROM ExtDocs", [], |row| row.get(0))?;
        assert_eq!(n, 1);

        Ok(())
    }

    #[test]
    fn consensus() -> Result<()> {
        use tor_netdoc::doc::netstatus;

        let (_tmp_dir, mut store) = new_empty()?;
        let now = Utc::now();
        let one_hour = CDuration::hours(1);

        assert_eq!(
            store.latest_consensus_time(ConsensusFlavor::Microdesc)?,
            None
        );

        let cmeta = ConsensusMeta::new(
            netstatus::Lifetime::new(
                now.into(),
                (now + one_hour).into(),
                (now + one_hour * 2).into(),
            )
            .unwrap(),
            [0xAB; 32],
            [0xBC; 32],
        );

        store.store_consensus(
            &cmeta,
            ConsensusFlavor::Microdesc,
            true,
            "Pretend this is a consensus",
        )?;

        {
            assert_eq!(
                store.latest_consensus_time(ConsensusFlavor::Microdesc)?,
                None
            );
            let consensus = store
                .latest_consensus(ConsensusFlavor::Microdesc, None)?
                .unwrap();
            assert_eq!(consensus.as_str()?, "Pretend this is a consensus");
            let consensus = store.latest_consensus(ConsensusFlavor::Microdesc, Some(false))?;
            assert!(consensus.is_none());
        }

        store.mark_consensus_usable(&cmeta)?;

        {
            assert_eq!(
                store.latest_consensus_time(ConsensusFlavor::Microdesc)?,
                now.into()
            );
            let consensus = store
                .latest_consensus(ConsensusFlavor::Microdesc, None)?
                .unwrap();
            assert_eq!(consensus.as_str()?, "Pretend this is a consensus");
            let consensus = store
                .latest_consensus(ConsensusFlavor::Microdesc, Some(false))?
                .unwrap();
            assert_eq!(consensus.as_str()?, "Pretend this is a consensus");
        }

        {
            let consensus_text = store.consensus_by_meta(&cmeta)?;
            assert_eq!(consensus_text.as_str()?, "Pretend this is a consensus");

            let (is, _cmeta2) = store
                .consensus_by_sha3_digest_of_signed_part(&[0xAB; 32])?
                .unwrap();
            assert_eq!(is.as_str()?, "Pretend this is a consensus");

            let cmeta3 = ConsensusMeta::new(
                netstatus::Lifetime::new(
                    now.into(),
                    (now + one_hour).into(),
                    (now + one_hour * 2).into(),
                )
                .unwrap(),
                [0x99; 32],
                [0x99; 32],
            );
            assert!(store.consensus_by_meta(&cmeta3).is_err());

            assert!(store
                .consensus_by_sha3_digest_of_signed_part(&[0x99; 32])?
                .is_none());
        }

        {
            assert!(store
                .consensus_by_sha3_digest_of_signed_part(&[0xAB; 32])?
                .is_some());
            store.delete_consensus(&cmeta)?;
            assert!(store
                .consensus_by_sha3_digest_of_signed_part(&[0xAB; 32])?
                .is_none());
        }

        Ok(())
    }

    #[test]
    fn authcerts() -> Result<()> {
        let (_tmp_dir, mut store) = new_empty()?;
        let now = Utc::now();
        let one_hour = CDuration::hours(1);

        let keyids = AuthCertKeyIds {
            id_fingerprint: [3; 20].into(),
            sk_fingerprint: [4; 20].into(),
        };
        let keyids2 = AuthCertKeyIds {
            id_fingerprint: [4; 20].into(),
            sk_fingerprint: [3; 20].into(),
        };

        let m1 = AuthCertMeta::new(keyids.clone(), now.into(), (now + one_hour * 24).into());

        store.store_authcerts(&[(m1, "Pretend this is a cert")])?;

        let certs = store.authcerts(&[keyids.clone(), keyids2])?;
        assert_eq!(certs.len(), 1);
        assert_eq!(certs.get(&keyids).unwrap(), "Pretend this is a cert");

        Ok(())
    }

    #[test]
    fn microdescs() -> Result<()> {
        let (_tmp_dir, mut store) = new_empty()?;

        let now = Utc::now();
        let one_day = CDuration::days(1);

        let d1 = [5_u8; 32];
        let d2 = [7; 32];
        let d3 = [42; 32];
        let d4 = [99; 32];

        store.store_microdescs(
            vec![
                ("Fake micro 1", &d1),
                ("Fake micro 2", &d2),
                ("Fake micro 3", &d3),
            ],
            (now - one_day * 100).into(),
        )?;

        store.update_microdescs_listed(&[d2], now.into())?;

        let mds = store.microdescs(&[d2, d3, d4])?;
        assert_eq!(mds.len(), 2);
        assert_eq!(mds.get(&d1), None);
        assert_eq!(mds.get(&d2).unwrap(), "Fake micro 2");
        assert_eq!(mds.get(&d3).unwrap(), "Fake micro 3");
        assert_eq!(mds.get(&d4), None);

        // Now we'll expire.  that should drop everything but d2.
        store.expire_all()?;
        let mds = store.microdescs(&[d2, d3, d4])?;
        assert_eq!(mds.len(), 1);
        assert_eq!(mds.get(&d2).unwrap(), "Fake micro 2");

        Ok(())
    }

    #[test]
    fn routerdescs() -> Result<()> {
        let (_tmp_dir, mut store) = new_empty()?;

        let now = Utc::now();
        let one_day = CDuration::days(1);
        let long_ago = now - one_day * 100;
        let recently = now - one_day;

        let d1 = [5_u8; 20];
        let d2 = [7; 20];
        let d3 = [42; 20];
        let d4 = [99; 20];

        store.store_routerdescs(vec![
            ("Fake routerdesc 1", long_ago.into(), &d1),
            ("Fake routerdesc 2", recently.into(), &d2),
            ("Fake routerdesc 3", long_ago.into(), &d3),
        ])?;

        let rds = store.routerdescs(&[d2, d3, d4])?;
        assert_eq!(rds.len(), 2);
        assert_eq!(rds.get(&d1), None);
        assert_eq!(rds.get(&d2).unwrap(), "Fake routerdesc 2");
        assert_eq!(rds.get(&d3).unwrap(), "Fake routerdesc 3");
        assert_eq!(rds.get(&d4), None);

        // Now we'll expire.  that should drop everything but d2.
        store.expire_all()?;
        let rds = store.routerdescs(&[d2, d3, d4])?;
        assert_eq!(rds.len(), 1);
        assert_eq!(rds.get(&d2).unwrap(), "Fake routerdesc 2");

        Ok(())
    }

    #[test]
    fn from_path_rw() -> Result<()> {
        let tmp = TempDir::new("arti-from-path").unwrap();

        // Nothing there: can't open read-only
        let r = SqliteStore::from_path(tmp.path(), true);
        assert!(r.is_err());
        assert!(!tmp.path().join("dir_blobs").exists());

        // Opening it read-write will crate the files
        {
            let mut store = SqliteStore::from_path(tmp.path(), false)?;
            assert!(tmp.path().join("dir_blobs").is_dir());
            assert!(store.lockfile.is_some());
            assert!(!store.is_readonly());
            assert!(store.upgrade_to_readwrite()?); // no-op.
        }

        // At this point, we can successfully make a read-only connection.
        {
            let mut store2 = SqliteStore::from_path(tmp.path(), true)?;
            assert!(store2.is_readonly());

            // Nobody else is locking this, so we can upgrade.
            assert!(store2.upgrade_to_readwrite()?); // no-op.
            assert!(!store2.is_readonly());
        }
        Ok(())
    }
}
