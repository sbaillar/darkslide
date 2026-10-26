//! The archive manager with an object-storage destination, against lt-archive's mock
//! S3 server behind the app's `ArchiveHttp` seam.

use lt_archive::object::mock::{self, MockS3};

use super::*;

/// The app's `URLSession` side, played by the mock server.
struct MockHttp(Arc<MockS3>);

impl ArchiveHttp for MockHttp {
    fn send(&self, r: ArchiveHttpRequest) -> Result<ArchiveHttpResponse, DsError> {
        let answer = lt_archive::object::Transport::send(
            &*self.0,
            lt_archive::object::Request {
                method: r.method,
                url: r.url,
                headers: r.headers.into_iter().map(|h| (h.name, h.value)).collect(),
                body: r.body,
            },
        )
        .map_err(|message| DsError::Io { message })?;
        Ok(ArchiveHttpResponse {
            status: answer.status,
            headers: answer
                .headers
                .into_iter()
                .map(|(name, value)| ArchiveHeader { name, value })
                .collect(),
            body: answer.body,
        })
    }
}

fn settings(prefix: &str) -> ObjectStoreSettings {
    let c = mock::config(prefix, true);
    ObjectStoreSettings {
        endpoint: c.endpoint,
        region: c.region,
        bucket: c.bucket,
        prefix: c.prefix,
        path_style: c.path_style,
    }
}

fn keys() -> ObjectCredentials {
    ObjectCredentials {
        access_key_id: mock::ACCESS_KEY.into(),
        secret_access_key: mock::SECRET.into(),
    }
}

/// Each test its own prefix: the archive lock is per destination and process-wide.
fn object_config(t: &Path, prefix: &str) -> ArchiveConfig {
    let mut cfg = config(&t.join("unused-folder"), t);
    cfg.object_store = Some(settings(prefix));
    cfg
}

#[test]
fn sessions_archive_to_object_storage() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "Été", &files[..1]);
    let s3 = Arc::new(MockS3::new());
    let m = manager(
        object_config(t.path(), "luxtable/"),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    // No keys yet: the target says so, nothing runs.
    m.set_object_store(Arc::new(MockHttp(s3.clone())), None)
        .unwrap();
    m.after_import(h.clone(), 1).unwrap();
    wait("target check", 10, || {
        m.status().unwrap().target.reason.as_deref() != Some("checking…")
    });
    let st = m.status().unwrap();
    assert!(!st.target.available);
    assert!(
        st.target
            .reason
            .as_deref()
            .unwrap()
            .contains("access key isn't set"),
        "{st:?}"
    );
    assert_eq!(st.target.kind.as_deref(), Some("object"));
    assert_eq!(s3.state().requests.len(), 0, "no request without keys");

    m.set_object_store(Arc::new(MockHttp(s3.clone())), Some(keys()))
        .unwrap();
    wait("object archive run", 30, || {
        runs(&m).iter().any(|r| r.outcome != "busy")
    });
    let run = &runs(&m)[0];
    assert_eq!(run.outcome, "succeeded", "{run:?}");
    let st = m.status().unwrap();
    assert!(st.target.available);
    assert_eq!(st.target.fs_type.as_deref(), Some("object storage"));
    assert_eq!(
        st.target.path,
        format!("s3://{}/{}/luxtable/", mock::HOST, mock::BUCKET)
    );
    assert_eq!(st.sessions[0].archive_folder.as_deref(), Some("Été"));
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    {
        let s = s3.state();
        let key = format!("luxtable/Été/Capture/{name}");
        assert_eq!(s.object(&key).unwrap().data, fs::read(&files[0]).unwrap());
        assert!(s.object("luxtable/Été/.lt/index-snapshot.db").is_some());
        assert!(
            !s.objects
                .keys()
                .any(|k| k.ends_with(".ltsession-wal") || k.contains(".lt/cache/")),
            "{:?}",
            s.objects.keys().collect::<Vec<_>>()
        );
        assert_eq!(s.object_deletes(), 0);
    }
    // The run's state is saved for the bucket, not the folder.
    let saved: serde_json::Value = serde_json::from_slice(
        &fs::read(PathBuf::from(h.root().unwrap()).join(STATE_REL)).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["target"], st.target.path);
    assert!(m.shutdown(5000).unwrap());
}

#[test]
fn object_storage_offline_and_refused_keys_are_reported_in_words() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let s3 = Arc::new(MockS3::new());
    s3.state().offline = true;
    let m = manager(
        object_config(t.path(), "offline/"),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.set_object_store(Arc::new(MockHttp(s3.clone())), Some(keys()))
        .unwrap();
    m.after_import(h, 1).unwrap();
    wait("offline check", 30, || {
        m.status()
            .unwrap()
            .target
            .reason
            .is_some_and(|r| r.contains("didn't answer"))
    });
    assert!(
        runs(&m).is_empty(),
        "nothing runs while the bucket is offline"
    );
    // Back, but with a wrong secret: the check tells the user.
    s3.state().offline = false;
    m.set_object_store(
        Arc::new(MockHttp(s3.clone())),
        Some(ObjectCredentials {
            access_key_id: mock::ACCESS_KEY.into(),
            secret_access_key: "wrong".into(),
        }),
    )
    .unwrap();
    wait("refused check", 30, || {
        m.status()
            .unwrap()
            .target
            .reason
            .is_some_and(|r| r.contains("refused the access key"))
    });
    assert!(m.shutdown(5000).unwrap());
    // Failure words for object storage.
    let root = PathBuf::from("s3://b/");
    assert_eq!(
        object_error(&ArchiveError::TargetUnavailable {
            root: root.clone(),
            reason: "x".into()
        })
        .as_deref(),
        Some("The storage service can't be reached.")
    );
    assert_eq!(
        object_error(&ArchiveError::TargetNotUsable {
            root,
            reason: "the bucket doesn't exist: b".into()
        })
        .as_deref(),
        Some("The bucket can't be used: the bucket doesn't exist: b.")
    );
    assert_eq!(object_error(&ArchiveError::Cancelled), None);
    assert_eq!(
        human_error(&ArchiveError::Storage("BadDigest".into()), None, || None),
        "The storage service refused the upload: BadDigest."
    );
}

#[test]
fn immich_album_sync_is_skipped_with_object_storage() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let s3 = Arc::new(MockS3::new());
    let m = manager(
        object_config(t.path(), "immich/"),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.set_immich_publish(Some(crate::immich::ArchivePublish {
        xmp_sidecars: false,
        label_tags: false,
    }));
    m.set_object_store(Arc::new(MockHttp(s3)), Some(keys()))
        .unwrap();
    m.after_import(h.clone(), 1).unwrap();
    wait("object archive run", 30, || !runs(&m).is_empty());
    assert_eq!(runs(&m)[0].outcome, "succeeded");
    assert!(!m.archives_to_folder());
    assert_eq!(m.immich_queued(), 0, "nothing queued for Immich");
    m.immich_requeue(&[h]);
    let (root, sessions) = m.immich_sessions(true);
    assert!(root.as_os_str().is_empty() && sessions.is_empty());
    assert_eq!(m.immich_queued(), 0);
    // Back to a folder: Immich has something to read again.
    let mut cfg = object_config(t.path(), "immich/");
    cfg.object_store = None;
    m.set_config(cfg).unwrap();
    assert!(m.archives_to_folder());
    assert!(m.shutdown(5000).unwrap());
}

/// Switching destinations never moves or deletes what is archived: the folder's
/// copy stays where it is, and the bucket gets its own.
#[test]
fn switching_destinations_keeps_both_archives() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "S", &files[..1]);
    let target = t.path().join("archive");
    fs::create_dir(&target).unwrap();
    let m = manager(
        config(&target, t.path()),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.after_import(h.clone(), 1).unwrap();
    wait("folder run", 30, || !runs(&m).is_empty());
    assert_eq!(runs(&m)[0].outcome, "succeeded", "{:?}", runs(&m));
    let folder_copy = dest_of(&m, &target, "S");
    let name = files[0].file_name().unwrap();
    assert!(folder_copy.join("Capture").join(name).is_file());

    let s3 = Arc::new(MockS3::new());
    m.set_object_store(Arc::new(MockHttp(s3.clone())), Some(keys()))
        .unwrap();
    let mut cfg = config(&target, t.path());
    cfg.object_store = Some(settings("switch/"));
    m.set_config(cfg).unwrap();
    m.sync_now().unwrap();
    wait("object run", 30, || runs(&m).len() >= 2);
    assert_eq!(runs(&m)[0].outcome, "succeeded", "{:?}", runs(&m));
    // The recorded folder name is kept on the bucket too.
    let folder = folder_copy
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let key = format!("switch/{folder}/Capture/{}", name.to_string_lossy());
    assert!(s3.state().object(&key).is_some(), "{key}");
    assert!(
        folder_copy.join("Capture").join(name).is_file(),
        "the folder copy stays"
    );
    assert!(m.shutdown(5000).unwrap());
}

#[test]
fn connection_test_through_the_app_http() {
    let s3 = Arc::new(MockS3::new());
    let http: Arc<dyn ArchiveHttp> = Arc::new(MockHttp(s3.clone()));
    let r = block_on(archive_object_test_connection(
        http.clone(),
        settings("luxtable"),
        keys(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(r.test_key, "luxtable/.lt-archive-test/connection-test.txt");
    assert!(r.location.starts_with("s3://photos/luxtable/"));
    assert!(s3.state().object(&r.test_key).is_some());
    let bridge = || -> Arc<dyn Transport> { Arc::new(HttpBridge(http.clone())) };
    let never = || false;
    // Wrong keys and bad settings are the user's to fix; no answer is I/O.
    let wrong = ObjectCredentials {
        access_key_id: "nope".into(),
        secret_access_key: "nope".into(),
    };
    assert!(
        format!("{wrong:?}").contains("nope")
            && !format!("{keys:?}", keys = keys()).contains(mock::SECRET)
    );
    assert!(matches!(
        test_object_store(bridge(), &settings(""), &wrong, &never),
        Err(DsError::InvalidArgument { .. })
    ));
    let mut bad = settings("");
    bad.bucket = "x".into();
    assert!(matches!(
        test_object_store(bridge(), &bad, &keys(), &never),
        Err(DsError::InvalidArgument { .. })
    ));
    s3.state().offline = true;
    assert!(matches!(
        test_object_store(bridge(), &settings(""), &keys(), &never),
        Err(DsError::Io { .. })
    ));
    assert!(matches!(
        test_object_store(bridge(), &settings(""), &keys(), &|| true),
        Err(DsError::Cancelled)
    ));
    s3.state().offline = false;
    s3.state().wrong_etag = true;
    assert!(matches!(
        test_object_store(bridge(), &settings(""), &keys(), &never),
        Err(DsError::Io { .. })
    ));
    let req = ArchiveHttpRequest {
        method: "GET".into(),
        url: "https://x/k".into(),
        headers: vec![ArchiveHeader {
            name: "Authorization".into(),
            value: "secret-ish".into(),
        }],
        body: vec![],
    };
    assert!(!format!("{req:?}").contains("secret-ish"));
}

#[test]
fn settings_problems_in_words() {
    assert_eq!(archive_object_settings_problem(settings("luxtable/")), None);
    let mut bad = settings("");
    bad.bucket = String::new();
    assert!(
        archive_object_settings_problem(bad)
            .unwrap()
            .contains("bucket")
    );
    let mut bad = settings("a/../b");
    assert!(
        archive_object_settings_problem(bad.clone())
            .unwrap()
            .contains("..")
    );
    bad.prefix = String::new();
    bad.endpoint = "ftp://x".into();
    assert!(
        archive_object_settings_problem(bad.clone())
            .unwrap()
            .contains("https")
    );
    // Plain http: a server on this network only.
    bad.endpoint = "http://s3.example.com".into();
    assert!(
        archive_object_settings_problem(bad.clone())
            .unwrap()
            .contains("use https://")
    );
    bad.endpoint = "http://192.168.1.20:9000".into();
    bad.path_style = true;
    assert_eq!(archive_object_settings_problem(bad), None);
}

/// The Amazon Photos hand-off doesn't depend on the archive destination: with object
/// storage (even one that is offline) imported originals still go to the folder.
#[test]
fn amazon_hand_off_works_with_object_storage() {
    let t = tempfile::tempdir().unwrap();
    let files = originals(t.path());
    let h = internal_session(t.path(), "Trip", &files[..2]);
    let s3 = Arc::new(MockS3::new());
    s3.state().offline = true;
    let m = manager(
        object_config(t.path(), "amazon/"),
        Arc::new(real_check),
        Arc::new(SystemRunner),
        PathBuf::from("/usr/bin/rsync"),
    );
    m.set_object_store(Arc::new(MockHttp(s3)), Some(keys()))
        .unwrap();
    let folder = t.path().join("Amazon Photos");
    fs::create_dir(&folder).unwrap();
    m.set_amazon(
        Some(folder.to_string_lossy().into_owned()),
        t.path()
            .join("support/m.json")
            .to_string_lossy()
            .into_owned(),
    )
    .unwrap();
    m.after_import(h, 2).unwrap();
    wait("handed over", 30, || {
        m.status().unwrap().sessions[0]
            .amazon
            .as_ref()
            .is_some_and(|a| a.placed == 2)
    });
    assert_eq!(fs::read_dir(folder.join("Trip")).unwrap().count(), 2);
    assert!(
        !m.status().unwrap().target.available,
        "the bucket is offline"
    );
    assert!(m.shutdown(5000).unwrap());
}
