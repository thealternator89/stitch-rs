use std::sync::{Arc, OnceLock};

use resvg::tiny_skia::{Color as SkiaColor, Pixmap, Transform};
use resvg::usvg;
use rusty_mermaid::{Theme as MermaidTheme, to_svg};

const RASTER_SCALE: f32 = 2.0;
const MAX_RASTER_DIM: f32 = 8192.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagramTheme {
    Light,
    Dark,
}

impl DiagramTheme {
    pub fn from_dark(is_dark: bool) -> Self {
        if is_dark {
            Self::Dark
        } else {
            Self::Light
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    pub fn to_mermaid_theme(self) -> MermaidTheme {
        match self {
            Self::Light => MermaidTheme::light(),
            Self::Dark => MermaidTheme::dark(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenderedDiagram {
    pub source: String,
    #[allow(dead_code)]
    pub theme: DiagramTheme,
    pub width: f32,
    pub height: f32,
    pub png_bytes: Vec<u8>,
}

fn has_font_family(db: &usvg::fontdb::Database, name: &str) -> bool {
    db.faces()
        .any(|face| face.families.iter().any(|(fam, _)| fam.eq_ignore_ascii_case(name)))
}

fn font_db() -> &'static Arc<usvg::fontdb::Database> {
    static FONT_DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONT_DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();

        for sans in [
            "Helvetica Neue",
            "Helvetica",
            "Arial",
            "Segoe UI",
            "DejaVu Sans",
            "Liberation Sans",
            "Noto Sans",
        ] {
            if has_font_family(&db, sans) {
                db.set_sans_serif_family(sans);
                break;
            }
        }

        for mono in ["SF Mono", "Menlo", "Consolas", "DejaVu Sans Mono"] {
            if has_font_family(&db, mono) {
                db.set_monospace_family(mono);
                break;
            }
        }

        Arc::new(db)
    })
}

/// Strips surrounding markdown code fences and normalizes line endings.
pub fn normalize_mermaid_source(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut trimmed = normalized.trim().to_string();

    if trimmed.starts_with("```") {
        if let Some(first_newline) = trimmed.find('\n') {
            trimmed = trimmed[first_newline + 1..].to_string();
        } else {
            return String::new();
        }
        trimmed = trimmed.trim().to_string();
        if let Some(stripped) = trimmed.strip_suffix("```") {
            trimmed = stripped.trim().to_string();
        }
    }

    trimmed
}

fn rasterize_svg_to_png(
    svg: &str,
    theme: DiagramTheme,
    scale: f32,
) -> Result<(f32, f32, Vec<u8>), String> {
    let options = usvg::Options {
        fontdb: Arc::clone(font_db()),
        font_family: "sans-serif".to_string(),
        ..Default::default()
    };

    let tree =
        usvg::Tree::from_str(svg, &options).map_err(|e| format!("Failed to parse rendered SVG: {e}"))?;

    let size = tree.size();
    let logical_w = if size.width().is_finite() && size.width() > 0.0 {
        size.width()
    } else {
        400.0
    };
    let logical_h = if size.height().is_finite() && size.height() > 0.0 {
        size.height()
    } else {
        300.0
    };

    let max_scale_x = MAX_RASTER_DIM / logical_w;
    let max_scale_y = MAX_RASTER_DIM / logical_h;
    let actual_scale = scale.min(max_scale_x).min(max_scale_y).max(0.1);

    let px_w = (logical_w * actual_scale).ceil().clamp(1.0, MAX_RASTER_DIM) as u32;
    let px_h = (logical_h * actual_scale).ceil().clamp(1.0, MAX_RASTER_DIM) as u32;

    let mut pixmap = Pixmap::new(px_w, px_h)
        .ok_or_else(|| format!("Failed to allocate {px_w}x{px_h} raster surface."))?;

    let bg = theme.to_mermaid_theme().background;
    pixmap.fill(SkiaColor::from_rgba8(bg.r, bg.g, bg.b, 255));

    resvg::render(
        &tree,
        Transform::from_scale(actual_scale, actual_scale),
        &mut pixmap.as_mut(),
    );

    let png_bytes = pixmap
        .encode_png()
        .map_err(|e| format!("Failed to encode PNG: {e}"))?;

    if png_bytes.is_empty() {
        return Err("Renderer produced empty PNG output.".to_string());
    }

    Ok((logical_w, logical_h, png_bytes))
}

/// Validates Mermaid syntax, computes layout dimensions, and renders a PNG preview.
pub fn validate_and_render(source: &str, theme: DiagramTheme) -> Result<RenderedDiagram, String> {
    let normalized = normalize_mermaid_source(source);
    if normalized.is_empty() {
        return Err("Diagram source is empty.".to_string());
    }

    let svg = to_svg(&normalized, &theme.to_mermaid_theme()).map_err(|e| e.to_string())?;
    let (width, height, png_bytes) = rasterize_svg_to_png(&svg, theme, RASTER_SCALE)?;

    Ok(RenderedDiagram {
        source: normalized,
        theme,
        width,
        height,
        png_bytes,
    })
}

/// Renders Mermaid source to SVG markup in the requested theme.
pub fn render_svg(source: &str, theme: DiagramTheme) -> Result<String, String> {
    let normalized = normalize_mermaid_source(source);
    if normalized.is_empty() {
        return Err("Diagram source is empty.".to_string());
    }
    to_svg(&normalized, &theme.to_mermaid_theme()).map_err(|e| e.to_string())
}

/// Renders Mermaid source to PNG bytes in the requested theme.
pub fn render_png(source: &str, theme: DiagramTheme) -> Result<Vec<u8>, String> {
    let svg = render_svg(source, theme)?;
    let (_, _, png_bytes) = rasterize_svg_to_png(&svg, theme, RASTER_SCALE)?;
    Ok(png_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_fenced_and_crlf_mermaid_source() {
        let raw = "  ```mermaid\r\nflowchart LR\r\n  A --> B\r\n```  ";
        assert_eq!(normalize_mermaid_source(raw), "flowchart LR\n  A --> B");
    }

    #[test]
    fn validates_and_renders_flowchart_and_sequence_diagrams() {
        let flowchart = "flowchart TD\n    A[Start] --> B[Finish]";
        let light = validate_and_render(flowchart, DiagramTheme::Light).expect("light flowchart");
        assert!(light.width > 0.0);
        assert!(light.height > 0.0);
        assert!(light.png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

        let seq = "sequenceDiagram\n    Alice->>Bob: Hello\n    Bob-->>Alice: Hi";
        let dark = validate_and_render(seq, DiagramTheme::Dark).expect("dark sequence");
        assert!(dark.width > 0.0);
        assert!(dark.height > 0.0);
        assert!(dark.png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn renders_light_and_dark_svg_and_png() {
        let src = "flowchart LR\n    A --> B";
        let light_svg = render_svg(src, DiagramTheme::Light).expect("light svg");
        let dark_svg = render_svg(src, DiagramTheme::Dark).expect("dark svg");
        assert!(light_svg.contains("<svg"));
        assert!(dark_svg.contains("<svg"));

        let light_png = render_png(src, DiagramTheme::Light).expect("light png");
        let dark_png = render_png(src, DiagramTheme::Dark).expect("dark png");
        assert!(light_png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(dark_png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn rejects_empty_and_invalid_mermaid_syntax() {
        assert!(validate_and_render("   ", DiagramTheme::Light).is_err());
        assert!(validate_and_render("not a valid diagram %%%%", DiagramTheme::Light).is_err());
    }
}
