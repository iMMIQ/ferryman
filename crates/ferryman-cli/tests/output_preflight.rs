//! Known output incompatibilities must fail before inference or model startup.
use std::{
    fs,
    io::Write,
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

fn docx(path: &Path) {
    let mut archive = zip::ZipWriter::new(fs::File::create(path).unwrap());
    archive
        .start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    archive.write_all(br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hello world</w:t></w:r></w:p></w:body></w:document>"#).unwrap();
    archive.finish().unwrap();
}

fn pdf(path: &Path) {
    use lopdf::{
        content::{Content, Operation},
        dictionary, Object, Stream,
    };
    let mut doc = lopdf::Document::with_version("1.7");
    let pages = doc.new_object_id();
    let font = doc.add_object(
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
    );
    let resources = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font } });
    let contents = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![50.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hello world")]),
            Operation::new("ET", vec![]),
        ],
    }
    .encode()
    .unwrap();
    let stream = doc.add_object(Stream::new(dictionary! {}, contents));
    let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 600.into(), 800.into()], "Resources" => resources, "Contents" => stream });
    doc.objects.insert(
        pages,
        dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1 }
            .into(),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", catalog);
    doc.save(path).unwrap();
}

#[tokio::test]
async fn invalid_fonts_never_call_the_model_and_docx_replace_works_in_mixed_batches() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap();
            let content = if prompt.contains("<c1>") { "<c1>translated</c1>" } else { "translated" };
            axum::Json(serde_json::json!({"choices":[{"finish_reason":"stop","message":{"content":content}}]}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("input.pdf");
    pdf(&fixture);
    let missing_font = root.path().join("missing-font.ttf");
    let corrupt_font = root.path().join("corrupt-font.ttf");
    fs::write(&corrupt_font, b"invalid font").unwrap();

    for (input, mode, fallback, expected) in [
        (
            fixture.clone(),
            "bilingual",
            Some(missing_font),
            "missing-font.ttf",
        ),
        (fixture.clone(), "replace", Some(corrupt_font), "font"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferryman"));
        command
            .args([
                "--no-cache",
                "--model",
                "fixture",
                "--target",
                "en",
                "--endpoint",
                &endpoint,
                "--mode",
                mode,
                "--input",
            ])
            .arg(input);
        if let Some(font) = fallback {
            command.env("FERRYMAN_PDF_FALLBACK_FONTS", font);
        }
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "unsupported output reached inference"
        );
    }
    assert!(!root.path().join("input.bilingual.pdf").exists());
    assert!(!root.path().join("input.translated.pdf").exists());

    let mixed = root.path().join("mixed");
    fs::create_dir(&mixed).unwrap();
    docx(&mixed.join("book.docx"));
    fs::write(mixed.join("book.txt"), "Source text").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferryman"));
    command
        .args([
            "--no-cache",
            "--model",
            "fixture",
            "--target",
            "en",
            "--endpoint",
            &endpoint,
            "--mode",
            "replace",
            "--input",
        ])
        .arg(&mixed);
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(mixed.join("book.translated.txt").exists());
    assert!(mixed.join("book.translated.docx").exists());
    let mut archive =
        zip::ZipArchive::new(fs::File::open(mixed.join("book.translated.docx")).unwrap()).unwrap();
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut archive.by_name("word/document.xml").unwrap(), &mut xml)
        .unwrap();
    assert!(xml.contains("translated") && !xml.contains("Hello world"));
    server.abort();
}

#[cfg(unix)]
#[test]
fn invalid_cli_formats_and_directory_options_do_not_start_containers() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let docker = root.path().join("docker");
    fs::write(
        &docker,
        "#!/bin/sh\ntouch \"$PREFLIGHT_FIXTURE/docker-called\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let source = root.path().join("input.rtf");
    fs::write(&source, "unsupported document").unwrap();
    for args in [
        vec![
            "--input".to_owned(),
            source.to_string_lossy().into_owned(),
            "--mode".into(),
            "replace".into(),
        ],
        vec![
            "--input".into(),
            root.path().to_string_lossy().into_owned(),
            "--output".into(),
            root.path()
                .join("output.txt")
                .to_string_lossy()
                .into_owned(),
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ferryman"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("PREFLIGHT_FIXTURE", root.path())
            .arg("--serve")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            !root.path().join("docker-called").exists(),
            "invalid CLI options started a model container"
        );
    }
}
