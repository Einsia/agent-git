use super::*;

#[test]
fn evidence_placeholder_text_is_never_reinterpreted_as_template_syntax() {
    let rendered = render_workflow([
        (
            "{{AUDIT_BINDING_JSON}}",
            json!({"literal":"{{REPORT_CONTRACT_JSON}}"}),
        ),
        (
            "{{PUBLICATION_MANIFEST_JSON}}",
            json!({"manifest":"frozen"}),
        ),
        ("{{READ_ACCESS_JSON}}", json!({"access":"local"})),
        ("{{REPORT_CONTRACT_JSON}}", json!({"contract":"bounded"})),
    ])
    .unwrap();
    assert!(rendered.contains("\"literal\": \"{{REPORT_CONTRACT_JSON}}\""));
    assert_eq!(rendered.matches("\"contract\": \"bounded\"").count(), 1);
}

#[test]
fn metadata_parts_preserve_long_unicode_strings_without_splitting_characters() {
    let root = tempfile::tempdir().unwrap();
    let mut inputs = Inputs::new(root.path());
    let value = json!({"text": "\u{1f600}".repeat(INPUT_CHUNK_BYTES)});
    let descriptor = inputs.json_parts("metadata", &value).unwrap();
    let mut bytes = Vec::new();
    for part in descriptor["parts"].as_array().unwrap() {
        let data = std::fs::read(part["path"].as_str().unwrap()).unwrap();
        assert!(data.len() <= INPUT_CHUNK_BYTES);
        assert!(std::str::from_utf8(&data).is_ok());
        assert_eq!(part["start_byte"].as_u64().unwrap(), bytes.len() as u64);
        bytes.extend(data);
        assert_eq!(part["end_byte"].as_u64().unwrap(), bytes.len() as u64);
    }
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
    inputs.verify().unwrap();
}

#[test]
fn changing_prepared_metadata_blocks_review_acceptance() {
    let root = tempfile::tempdir().unwrap();
    let mut inputs = Inputs::new(root.path());
    let path = inputs.write("trusted.json", b"original").unwrap();
    inputs.verify().unwrap();
    std::fs::write(&path, b"modified").unwrap();
    assert!(inputs.verify().is_err());
    assert!(inputs.write("trusted.json", b"replace").is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"modified");
}

#[test]
fn report_files_must_be_regular_available_and_within_the_raw_limit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("report.json");
    assert!(read_report(&path).is_err());
    std::fs::create_dir(&path).unwrap();
    assert!(read_report(&path).is_err());
    std::fs::remove_dir(&path).unwrap();
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_REPORT_BYTES as u64 + 1).unwrap();
    assert!(read_report(&path).is_err());
    std::fs::write(&path, b"{}").unwrap();
    assert_eq!(read_report(&path).unwrap(), b"{}");
}

#[cfg(unix)]
#[test]
fn report_and_input_symlinks_do_not_redirect_reads() {
    let root = tempfile::tempdir().unwrap();
    let mut inputs = Inputs::new(root.path());
    let path = inputs.write("trusted.json", b"expected").unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, b"expected").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(inputs.verify().is_err());
    assert!(read_report(&path).is_err());
}
