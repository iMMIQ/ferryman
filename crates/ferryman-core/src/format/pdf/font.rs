use super::sfnt::{extract_face, face_is_cff};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ── font discovery + sharing ────────────────────────────────────────────────

/// A CJK-capable OpenType font shared by every PDF document in the process.
#[derive(Clone)]
pub struct PdfFont {
    /// The selected face re-packed as a standalone sfnt (`Arc` so all
    /// clones share one copy; faces are extracted out of `.ttc` collections
    /// because a PDF FontFile may not be a collection).
    pub(super) standalone: Arc<Vec<u8>>,
    /// CFF-flavored OpenType (chooses CIDFontType0/FontFile3 embedding, vs
    /// TrueType outlines' CIDFontType2/FontFile2).
    pub(super) cff: bool,
}

impl PdfFont {
    /// The process-wide shared font, loaded on first use. The first existing
    /// file wins: `$FERRYMAN_PDF_FONT`, the Docker image's `/app/fonts`, then
    /// the usual distro locations for Noto CJK / WenQuanYi / Droid fallback.
    pub fn shared() -> Result<&'static Self> {
        static SHARED: OnceLock<PdfFont> = OnceLock::new();
        if let Some(f) = SHARED.get() {
            return Ok(f);
        }
        let candidates = candidate_font_paths();
        let path = candidates.iter().find(|p| p.is_file()).ok_or_else(|| {
            anyhow!(
                "no PDF font found (looked in: {}); install fonts-noto-cjk \
                 or point FERRYMAN_PDF_FONT at a CJK OpenType font (.ttc/.otf/.ttf)",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let font = Self::load(path).with_context(|| format!("load pdf font {}", path.display()))?;
        Ok(SHARED.get_or_init(|| font))
    }

    pub(super) fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let index = preferred_face_index(&data);
        ttf_parser::Face::parse(&data, index)
            .map_err(|error| anyhow!("invalid PDF font {}: {error:?}", path.display()))?;
        let standalone = Arc::new(extract_face(&data, index));
        ttf_parser::Face::parse(&standalone, 0)
            .map_err(|error| anyhow!("invalid extracted PDF font {}: {error:?}", path.display()))?;
        let cff = face_is_cff(&standalone);
        Ok(PdfFont { standalone, cff })
    }

    /// A krilla font handle over the standalone face (test fixtures only).
    #[cfg(test)]
    pub(super) fn krilla(&self) -> Option<krilla::text::Font> {
        krilla::text::Font::new(Arc::clone(&self.standalone).into(), 0)
    }
}

/// Where to look for a CJK font, most specific first. See [`PdfFont::shared`].
pub(super) fn candidate_font_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(env) = std::env::var_os("FERRYMAN_PDF_FONT") {
        if !env.is_empty() {
            paths.push(PathBuf::from(env));
        }
    }
    paths.extend(
        [
            // Docker image (scripts/build-release.sh bundles it here).
            "/app/fonts/NotoSansCJK-Regular.ttc",
            // Debian/Ubuntu (fonts-noto-cjk).
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            // Fedora / Arch packaging of the same face.
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            // Serif variant: better fits book typography, still fine.
            "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
            // Common distro fallbacks.
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/droid/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/droid/DroidSansFallback.ttf",
        ]
        .map(PathBuf::from),
    );
    paths
}

/// Pick the face to use inside a `.ttc`: prefer the Simplified-Chinese face
/// (the default target language), then any CJK face, then face 0. Standalone
/// fonts parse only at index 0.
pub(super) fn preferred_face_index(data: &[u8]) -> u32 {
    let mut first_cjk: Option<u32> = None;
    for i in 0.. {
        let Ok(face) = ttf_parser::Face::parse(data, i) else {
            break;
        };
        let name = face_name(&face);
        if name.contains("CJK SC") || name.contains(" SC") {
            return i;
        }
        if first_cjk.is_none() && name.contains("CJK") {
            first_cjk = Some(i);
        }
    }
    first_cjk.unwrap_or(0)
}

/// The font's English full face name ("" when absent), e.g. "Noto Sans CJK SC".
pub(super) fn face_name(face: &ttf_parser::Face<'_>) -> String {
    face.names()
        .into_iter()
        .filter(|n| n.name_id == ttf_parser::name::name_id::FULL_NAME && n.is_unicode())
        .find_map(|n| n.to_string())
        .unwrap_or_default()
}
