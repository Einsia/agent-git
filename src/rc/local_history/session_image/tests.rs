use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

/// A reply names the images the page's CommonMark renderer shows, and the CodeLark tag counts only
/// in its exact image form and outside code. A pattern match over the text escapes this: it counts
/// an image or a tag quoted in code, or an image nested in another's alt text, and misses an
/// escaped, entity-coded, reference-style or footnoted destination, so the device serves images
/// the page does not show and refuses ones it does.
#[test]
fn a_reply_names_the_images_the_page_renders() {
    let cases: &[(&str, &[&str])] = &[
        (
            r#"![a](shot.png) and ![b](<with space.png> "title")"#,
            &["shot.png", "with space.png"],
        ),
        (r"![a](still\_frame&#46;webp)", &["still_frame.webp"]),
        (
            "![a][ref]\n\n[ref]: by/reference.png",
            &["by/reference.png"],
        ),
        ("Note.[^1]\n\n[^1]: ![a](foot.png)", &["foot.png"]),
        ("![![inner](private.png)](shown.png)", &["shown.png"]),
        (
            r#"![<clk-send>{"msg_type":"image","local_path":"/abs/alt.png"}</clk-send>](shown.png)"#,
            &["shown.png"],
        ),
        (
            "`![a](span.png)`\n\n```\n![a](fence.png)\n```\n\n    ![a](indented.png)",
            &[],
        ),
        (r#"[a link](link.png) <img src="html.png">"#, &[]),
        (
            r#"<clk-send>{"msg_type":"image","local_path":"/abs/tag.png"}</clk-send>"#,
            &["/abs/tag.png"],
        ),
        (
            r#"<clk-send>{"msg_type":"file","local_path":"/abs/file.png"}</clk-send>"#,
            &[],
        ),
        (
            r#"<clk-send>{"msg_type":"image","local_path":"/abs/x.png","caption":"x"}</clk-send>"#,
            &[],
        ),
        (
            r#"<clk-send>{"msg_type":"image","local_path":7}</clk-send>"#,
            &[],
        ),
        (
            r#"<clk-send>{"msg_type":"image","local_path":"/abs/open.png"}"#,
            &[],
        ),
        (
            "`<clk-send>{\"msg_type\":\"image\",\"local_path\":\"/abs/span.png\"}</clk-send>`\n\n\
             ```\n<clk-send>{\"msg_type\":\"image\",\"local_path\":\"/abs/fence.png\"}</clk-send>\n```",
            &[],
        ),
    ];
    for (reply, expected) in cases {
        assert_eq!(named_images(reply), *expected, "{reply}");
    }
}

/// One request end to end, through the session lookup and its transcript: a file is returned only
/// when an agent reply of this session shows it, it lies inside the session's folder once links
/// are resolved, its name and header agree on a raster type, and it fits the cap; the type
/// returned is the header's. Each refused row is what a plausible shortcut serves: matching any
/// transcript text or only the newest page, comparing paths without resolving links, trusting the
/// extension, or cutting a large file short. A path no reply names is refused alike whether or not
/// it exists, and no refusal carries the path.
#[test]
fn a_session_image_is_read_only_as_an_agent_reply_of_the_session_shows_it() {
    let directory = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(directory.path(), || {
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        let outside = root.join("outside");
        for folder in [&project, &outside] {
            std::fs::create_dir(folder).unwrap();
        }
        let png = [PNG, b"pixels".as_slice()].concat();
        let huge = [
            PNG,
            vec![0; crate::protocol::FILE_PREVIEW_CAP as usize].as_slice(),
        ]
        .concat();
        let files: [(&str, &[u8]); 16] = [
            ("shot.png", &png),
            ("photo.jpg", b"\xff\xd8\xff\xe0jpeg"),
            ("anim.gif", b"GIF89a gif"),
            ("still_frame.webp", b"RIFF\x04\0\0\0WEBPVP8 "),
            ("said.png", &png),
            ("listed.png", &png),
            ("quiet.png", &png),
            ("quoted.png", &png),
            ("attached.png", &png),
            ("fenced.png", &png),
            ("notes.png", b"plain text"),
            ("swapped.gif", &png),
            (
                "vector.svg",
                br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#,
            ),
            ("huge.png", &huge),
            ("../outside/away.png", &png),
            ("../outside/elsewhere.png", &png),
        ];
        for (name, bytes) in files {
            std::fs::write(project.join(name), bytes).unwrap();
        }
        let away = outside.join("away.png");
        let markdown_path = |path: &std::path::Path| path.to_string_lossy().replace('\\', "/");
        let tag = |path: &std::path::Path| serde_json::to_string(&path.to_string_lossy()).unwrap();
        let native = "00000000-0000-0000-0000-0000000000a1";
        let message = |role: &str, kind: &str, text: &str| {
            json!({"type":"response_item","payload":{
                "type":"message","role":role,"content":[{"type":kind,"text":text}]
            }})
        };
        let reply = format!(
            "![photo](<photo.jpg>) ![still](still\\_frame&#46;webp) `![quoted](quoted.png)`\n\n\
             ```\n![quoted](quoted.png)\n```\n\n\
             ![away]({}) ![link](link.png) ![notes](notes.png) ![swapped](swapped.gif) \
             ![vector](vector.svg) ![huge](huge.png) ![gone](gone.png)\n\n\
             <clk-send>{{\"msg_type\":\"image\",\"local_path\":{}}}</clk-send>\n\
             <clk-send>{{\"msg_type\":\"file\",\"local_path\":{}}}</clk-send>\n\n\
             ```\n<clk-send>{{\"msg_type\":\"image\",\"local_path\":{}}}</clk-send>\n```",
            markdown_path(&away),
            tag(&project.join("anim.gif")),
            tag(&project.join("attached.png")),
            tag(&project.join("fenced.png")),
        );
        let mut records = vec![
            json!({"type":"session_meta","payload":{"id":native,"cwd":project}}),
            message("assistant", "output_text", "Here it is: ![shot](shot.png)"),
            message(
                "user",
                "input_text",
                "Look at ![said](said.png) and ![elsewhere](../outside/elsewhere.png)",
            ),
            json!({"type":"response_item","payload":{
                "type":"function_call_output","call_id":"c1","output":"![listed](listed.png)\nlisted.png"
            }}),
            message("assistant", "output_text", &reply),
        ];
        // The replies that name images lie before the newest history page.
        records.extend((0..70).map(|_| message("assistant", "output_text", "Done.")));
        let home = root.join("codex");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let transcript = home.join(format!("sessions/rollout-{native}.jsonl"));
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        std::fs::write(&transcript, text).unwrap();
        let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, archived INTEGER, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1,?2,?3,0,'question','cli',1)",
            rusqlite::params![native, transcript.to_str(), project.to_str()],
        )
        .unwrap();
        let source = crate::rc::runtime_sources::Registry::open()
            .unwrap()
            .register(&home, None, None, None)
            .unwrap();
        let mut roster = crate::rc::roster::Roster::default();
        roster
            .record(
                "image",
                serde_json::from_value(json!({
                    "native_source":{"source_id":source.source_id,"generation":source.generation},
                    "runtime":"codex","thread_id":native,"cwd":project,"workspace_id":"local-owner"
                }))
                .unwrap(),
            )
            .unwrap();

        let absolute = |path: std::path::PathBuf| path.to_string_lossy().into_owned();
        #[allow(unused_mut)]
        let mut rows: Vec<(String, Result<&str, &str>)> = vec![
            ("shot.png".into(), Ok("image/png")),
            (absolute(project.join("photo.jpg")), Ok("image/jpeg")),
            ("anim.gif".into(), Ok("image/gif")),
            ("still_frame.webp".into(), Ok("image/webp")),
            ("said.png".into(), Err("not_named")),
            ("listed.png".into(), Err("not_named")),
            ("quiet.png".into(), Err("not_named")),
            ("nowhere.png".into(), Err("not_named")),
            ("quoted.png".into(), Err("not_named")),
            ("attached.png".into(), Err("not_named")),
            ("fenced.png".into(), Err("not_named")),
            (absolute(outside.join("elsewhere.png")), Err("not_named")),
            (absolute(away.clone()), Err("outside_folder")),
            ("notes.png".into(), Err("not_image")),
            ("swapped.gif".into(), Err("not_image")),
            ("vector.svg".into(), Err("not_image")),
            ("huge.png".into(), Err("too_large")),
            ("gone.png".into(), Err("missing")),
        ];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&away, project.join("link.png")).unwrap();
            std::os::unix::fs::symlink(&project, root.join("alias")).unwrap();
            rows.push(("link.png".into(), Err("outside_folder")));
            // Another spelling of a named file is the same file.
            rows.push((absolute(root.join("alias/shot.png")), Ok("image/png")));
        }
        for (path, expected) in rows {
            let result = read_with_roster(&json!({"session_id":"image","path":path}), &roster);
            match expected {
                Ok(mime) => {
                    use base64::Engine as _;
                    let image = result.unwrap_or_else(|error| panic!("{path}: {error}"));
                    assert_eq!(image["mime"], mime, "{path}");
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(image["base64"].as_str().unwrap())
                        .unwrap();
                    assert_eq!(
                        bytes,
                        std::fs::read(resolve(&project, &path)).unwrap(),
                        "{path}"
                    );
                    assert_eq!(image["size"], bytes.len(), "{path}");
                }
                Err(kind) => {
                    let error = rpc_error(result.expect_err(&path));
                    assert_eq!(error.data.as_ref().unwrap()["kind"], kind, "{path}");
                    let shown = serde_json::to_string(&error).unwrap();
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy();
                    assert!(
                        !shown.contains(&*name) && !shown.contains(&*root.to_string_lossy()),
                        "{shown}"
                    );
                }
            }
        }
    });
}
