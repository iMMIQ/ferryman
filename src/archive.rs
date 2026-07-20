//! EPUB (ZIP) read/write + OPF manifest parsing.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
    pub method: CompressionMethod,
    pub is_dir: bool,
}

pub struct Epub {
    pub entries: Vec<Entry>,
}

impl Epub {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut za = ZipArchive::new(file).context("read zip")?;
        let mut entries = Vec::with_capacity(za.len() as usize);
        for i in 0..za.len() {
            let mut zf = za.by_index(i)?;
            let name = zf.name().to_string();
            let is_dir = zf.is_dir();
            let method = zf.compression();
            let mut data = Vec::new();
            zf.read_to_end(&mut data)?;
            entries.push(Entry {
                name,
                data,
                method,
                is_dir,
            });
        }
        Ok(Epub { entries })
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.name == name)
    }

    pub fn read_text(&self, name: &str) -> Result<String> {
        let idx = self
            .find(name)
            .ok_or_else(|| anyhow!("entry not found: {}", name))?;
        Ok(String::from_utf8_lossy(&self.entries[idx].data).into_owned())
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut zw = ZipWriter::new(file);

        // EPUB validity: mimetype must be the first entry, uncompressed, no extra.
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflate = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        if let Some(m) = self.entries.iter().find(|e| e.name == "mimetype") {
            zw.start_file("mimetype", stored)?;
            zw.write_all(&m.data)?;
        }

        for e in self.entries.iter().filter(|e| e.name != "mimetype") {
            if e.is_dir {
                zw.add_directory(&e.name, deflate)?;
            } else {
                // Preserve each entry's original compression where possible.
                let opts = SimpleFileOptions::default().compression_method(e.method);
                zw.start_file(&e.name, opts)?;
                zw.write_all(&e.data)?;
            }
        }
        zw.finish().context("finish zip")?;
        Ok(())
    }

    /// Resolve the ordered list of spine content (xhtml) file paths within the
    /// archive, excluding the navigation document.
    pub fn content_files(&self) -> Result<Vec<String>> {
        let container = self
            .read_text("META-INF/container.xml")
            .context("EPUB missing META-INF/container.xml")?;
        let doc = roxmltree::Document::parse(&container)?;
        let opf_path = doc
            .descendants()
            .find(|n| n.has_tag_name("rootfile"))
            .and_then(|n| n.attribute("full-path"))
            .ok_or_else(|| anyhow!("container.xml has no rootfile/@full-path"))?
            .to_string();

        let opf_dir = parent_dir(&opf_path);
        let opf_xml = self
            .read_text(&opf_path)
            .with_context(|| format!("read OPF {}", opf_path))?;
        let opf = roxmltree::Document::parse(&opf_xml).context("parse OPF")?;

        // id -> (href, media-type, properties)
        let mut items: HashMap<String, (String, String, String)> = HashMap::new();
        for n in opf.descendants().filter(|n| n.has_tag_name("item")) {
            let id = match n.attribute("id") {
                Some(v) => v.to_string(),
                None => continue,
            };
            let href = n.attribute("href").unwrap_or("").to_string();
            let mt = n.attribute("media-type").unwrap_or("").to_string();
            let props = n.attribute("properties").unwrap_or("").to_string();
            items.insert(id, (href, mt, props));
        }

        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for n in opf.descendants().filter(|n| n.has_tag_name("itemref")) {
            let idref = match n.attribute("idref") {
                Some(v) => v,
                None => continue,
            };
            let (href, mt, props) = match items.get(idref) {
                Some(v) => v,
                None => continue,
            };
            if mt != "application/xhtml+xml" {
                continue;
            }
            // Skip the navigation document.
            if props.split_whitespace().any(|p| p == "nav") {
                continue;
            }
            let archive = join_path(&opf_dir, href);
            if seen.insert(archive.clone()) {
                result.push(archive);
            }
        }

        if result.is_empty() {
            return Err(anyhow!(
                "OPF spine yielded no xhtml content items; check {}",
                opf_path
            ));
        }
        Ok(result)
    }
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// Join an OPF-relative href onto the OPF directory, normalising separators.
fn join_path(dir: &str, href: &str) -> String {
    let href = href.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(href.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(seg),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_cases() {
        assert_eq!(parent_dir("OEBPS/content.opf"), "OEBPS");
        assert_eq!(parent_dir("a/b/c.opf"), "a/b");
        assert_eq!(parent_dir("content.opf"), "");
        assert_eq!(parent_dir("/abs.opf"), "");
    }

    #[test]
    fn join_path_basic() {
        assert_eq!(join_path("OEBPS", "ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(join_path("", "ch1.xhtml"), "ch1.xhtml");
    }

    #[test]
    fn join_path_parent_reference() {
        assert_eq!(join_path("OEBPS", "../ch1.xhtml"), "ch1.xhtml");
        assert_eq!(join_path("a/b", "../c.xhtml"), "a/c.xhtml");
    }

    #[test]
    fn join_path_normalises_separators_and_dot() {
        assert_eq!(join_path("OEBPS", "sub\\ch1.xhtml"), "OEBPS/sub/ch1.xhtml");
        assert_eq!(join_path("a/b", "c/./d.xhtml"), "a/b/c/d.xhtml");
    }
}
