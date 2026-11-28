use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, ThemeMode as UiThemeMode, h_flex, v_flex,
};
use gpui_kit::{
    App, Context, ElementId, Image, ImageFormat, IntoElement, ObjectFit, ParentElement as _,
    Render, RenderOnce, SharedString, Styled as _, StyledImage as _, Window, div, hsla, img, px,
};

use crate::features::diagram_builder::renderer::{
    DiagramTheme, normalize_mermaid_source, validate_and_render,
};
use crate::infrastructure::runtime::spawn_tokio;

const MAX_CACHE_ENTRIES: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MarkdownSegment {
    Markdown(String),
    Mermaid(String),
}

enum FenceState {
    None,
    InOtherFence {
        ch: char,
        len: usize,
    },
    InMermaidFence {
        ch: char,
        len: usize,
        opening_line: String,
        body_lines: Vec<String>,
    },
}

/// Parses a line as a CommonMark opening code fence (`0..=3` spaces followed by
/// `>= 3` backticks or tildes). Returns `(fence_char, fence_len, info_string)`.
fn parse_opening_fence(line: &str) -> Option<(char, usize, &str)> {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let rest = &line[leading_spaces..];
    let mut chars = rest.chars();
    let ch = chars.next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = &rest[len..];
    // Per CommonMark, backtick fence info strings cannot contain backticks.
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

fn is_closing_fence(line: &str, expected_ch: char, min_len: usize) -> bool {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return false;
    }
    let rest = &line[leading_spaces..];
    let count = rest.chars().take_while(|&c| c == expected_ch).count();
    if count < min_len {
        return false;
    }
    rest[count..].trim().is_empty()
}

fn is_mermaid_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
}

fn push_md_line(buf: &mut String, line: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(line);
}

fn flush_md_buf(buf: &mut String, segments: &mut Vec<MarkdownSegment>) {
    if !buf.trim().is_empty() {
        segments.push(MarkdownSegment::Markdown(std::mem::take(buf)));
    } else {
        buf.clear();
    }
}

/// Splits Markdown into prose/code segments and standalone fenced `mermaid` blocks.
///
/// Nested ` ```mermaid ` examples inside outer fences (e.g. ````markdown`) are
/// preserved verbatim as Markdown, and unclosed `mermaid` fences remain Markdown.
pub(crate) fn split_markdown_segments(input: &str) -> Vec<MarkdownSegment> {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut segments = Vec::new();
    let mut md_buf = String::new();
    let mut state = FenceState::None;

    for line in normalized.split('\n') {
        match &mut state {
            FenceState::None => {
                if let Some((ch, len, info)) = parse_opening_fence(line) {
                    if is_mermaid_info(info) {
                        flush_md_buf(&mut md_buf, &mut segments);
                        state = FenceState::InMermaidFence {
                            ch,
                            len,
                            opening_line: line.to_string(),
                            body_lines: Vec::new(),
                        };
                    } else {
                        push_md_line(&mut md_buf, line);
                        state = FenceState::InOtherFence { ch, len };
                    }
                } else {
                    push_md_line(&mut md_buf, line);
                }
            }
            FenceState::InOtherFence { ch, len } => {
                let (expected_ch, min_len) = (*ch, *len);
                push_md_line(&mut md_buf, line);
                if is_closing_fence(line, expected_ch, min_len) {
                    state = FenceState::None;
                }
            }
            FenceState::InMermaidFence {
                ch,
                len,
                body_lines,
                ..
            } => {
                let (expected_ch, min_len) = (*ch, *len);
                if is_closing_fence(line, expected_ch, min_len) {
                    segments.push(MarkdownSegment::Mermaid(body_lines.join("\n")));
                    state = FenceState::None;
                } else {
                    body_lines.push(line.to_string());
                }
            }
        }
    }

    if let FenceState::InMermaidFence {
        opening_line,
        body_lines,
        ..
    } = state
    {
        if let Some(MarkdownSegment::Markdown(prev)) = segments.last_mut()
            && md_buf.is_empty()
        {
            push_md_line(prev, &opening_line);
            for line in body_lines {
                push_md_line(prev, &line);
            }
        } else {
            push_md_line(&mut md_buf, &opening_line);
            for line in body_lines {
                push_md_line(&mut md_buf, &line);
            }
        }
    }

    flush_md_buf(&mut md_buf, &mut segments);
    segments
}

#[derive(Clone)]
enum CachedDiagram {
    Ready {
        image: Arc<Image>,
        width: f32,
        height: f32,
    },
    Error(String),
}

struct MermaidRenderCache {
    max_entries: usize,
    order: VecDeque<(String, DiagramTheme)>,
    map: HashMap<(String, DiagramTheme), CachedDiagram>,
}

impl MermaidRenderCache {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    fn get(&self, key: &(String, DiagramTheme)) -> Option<CachedDiagram> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: (String, DiagramTheme), value: CachedDiagram) {
        if let Some(existing) = self.map.get_mut(&key) {
            *existing = value;
            return;
        }
        while self.map.len() >= self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }
}

fn global_cache() -> &'static Mutex<MermaidRenderCache> {
    static MERMAID_CACHE: OnceLock<Mutex<MermaidRenderCache>> = OnceLock::new();
    MERMAID_CACHE.get_or_init(|| Mutex::new(MermaidRenderCache::new(MAX_CACHE_ENTRIES)))
}

fn get_cached_diagram(key: &(String, DiagramTheme)) -> Option<CachedDiagram> {
    global_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(key))
}

fn insert_cached_diagram(key: (String, DiagramTheme), value: CachedDiagram) {
    if let Ok(mut cache) = global_cache().lock() {
        cache.insert(key, value);
    }
}

enum BlockRenderStatus {
    Idle,
    Loading,
    Ready {
        image: Arc<Image>,
        width: f32,
        #[allow(dead_code)]
        height: f32,
    },
    Error(String),
}

struct MermaidBlockView {
    source: String,
    theme: DiagramTheme,
    selectable: bool,
    fallback_id: SharedString,
    status: BlockRenderStatus,
    render_gen: u64,
}

impl MermaidBlockView {
    fn new(fallback_id: SharedString) -> Self {
        Self {
            source: String::new(),
            theme: DiagramTheme::Light,
            selectable: false,
            fallback_id,
            status: BlockRenderStatus::Idle,
            render_gen: 0,
        }
    }

    fn sync(
        &mut self,
        source: String,
        theme: DiagramTheme,
        selectable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selectable = selectable;

        if self.source == source
            && self.theme == theme
            && !matches!(self.status, BlockRenderStatus::Idle)
        {
            return;
        }

        self.source = source.clone();
        self.theme = theme;

        let normalized = normalize_mermaid_source(&source);
        let key = (normalized, theme);

        if let Some(cached) = get_cached_diagram(&key) {
            self.status = match cached {
                CachedDiagram::Ready {
                    image,
                    width,
                    height,
                } => BlockRenderStatus::Ready {
                    image,
                    width,
                    height,
                },
                CachedDiagram::Error(err) => BlockRenderStatus::Error(err),
            };
            return;
        }

        self.status = BlockRenderStatus::Loading;
        self.render_gen = self.render_gen.wrapping_add(1);
        let gen_id = self.render_gen;
        let view = cx.entity();

        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move { validate_and_render(&source, theme) }).await;
                let cached = match res {
                    Ok(Ok(rendered)) => CachedDiagram::Ready {
                        image: Arc::new(Image::from_bytes(ImageFormat::Png, rendered.png_bytes)),
                        width: rendered.width,
                        height: rendered.height,
                    },
                    Ok(Err(err)) => CachedDiagram::Error(err),
                    Err(join_err) => {
                        CachedDiagram::Error(format!("Mermaid render task failed: {join_err}"))
                    }
                };

                insert_cached_diagram(key, cached.clone());

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.render_gen != gen_id {
                            return;
                        }
                        this.status = match cached {
                            CachedDiagram::Ready {
                                image,
                                width,
                                height,
                            } => BlockRenderStatus::Ready {
                                image,
                                width,
                                height,
                            },
                            CachedDiagram::Error(err) => BlockRenderStatus::Error(err),
                        };
                        cx.notify();
                    });
                });
            })
            .detach();
    }
}

impl Render for MermaidBlockView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let canvas_bg = if self.theme.is_dark() {
            hsla(215.0 / 360.0, 0.21, 0.11, 1.0)
        } else {
            gpui_kit::white()
        };

        match &self.status {
            BlockRenderStatus::Ready { image, width, .. } => div()
                .w_full()
                .flex()
                .justify_center()
                .p_4()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(canvas_bg)
                .overflow_hidden()
                .child(
                    img(image.clone())
                        .w(px(*width))
                        .max_w_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            BlockRenderStatus::Idle | BlockRenderStatus::Loading => h_flex()
                .w_full()
                .items_center()
                .justify_center()
                .gap_2()
                .py_6()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small().color(theme.primary))
                .child("Rendering diagram...")
                .into_any_element(),
            BlockRenderStatus::Error(err) => {
                let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
                let fenced = format!("```mermaid\n{}\n```", self.source);
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(amber.opacity(0.35))
                    .bg(theme.background)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .text_xs()
                            .text_color(amber)
                            .child(Icon::new(LucideIcon::TriangleAlert).xsmall())
                            .child(div().flex_1().min_w_0().child(format!(
                                "Unable to render Mermaid diagram: {err}"
                            ))),
                    )
                    .child(
                        TextView::markdown(self.fallback_id.clone(), fenced)
                            .selectable(self.selectable),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Renders Markdown content, rendering any fenced `mermaid` code blocks inline
/// via `rusty-mermaid` while delegating surrounding Markdown to `TextView::markdown`.
#[derive(IntoElement)]
pub struct MarkdownView {
    id: ElementId,
    text: SharedString,
    selectable: bool,
}

impl MarkdownView {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            selectable: false,
        }
    }

    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = selectable;
        self
    }
}

impl RenderOnce for MarkdownView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let segments = split_markdown_segments(self.text.as_ref());
        let has_mermaid = segments
            .iter()
            .any(|seg| matches!(seg, MarkdownSegment::Mermaid(_)));

        if !has_mermaid {
            return TextView::markdown(self.id, self.text)
                .selectable(self.selectable)
                .into_any_element();
        }

        let diagram_theme = DiagramTheme::from_dark(matches!(cx.theme().mode, UiThemeMode::Dark));
        let mut container = v_flex().w_full().min_w_0().gap_3();

        for (idx, segment) in segments.into_iter().enumerate() {
            let seg_id: SharedString = format!("{:?}-seg-{idx}", self.id).into();
            match segment {
                MarkdownSegment::Markdown(md) => {
                    container = container
                        .child(TextView::markdown(seg_id, md).selectable(self.selectable));
                }
                MarkdownSegment::Mermaid(code) => {
                    let fallback_id: SharedString =
                        format!("{:?}-mmd-fallback-{idx}", self.id).into();
                    let block_view = window.use_keyed_state(seg_id, cx, |_window, _cx| {
                        MermaidBlockView::new(fallback_id)
                    });
                    block_view.update(cx, |view, cx| {
                        view.sync(code, diagram_theme, self.selectable, window, cx);
                    });
                    container = container.child(block_view);
                }
            }
        }

        container.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_into_cache(
        cache: &mut MermaidRenderCache,
        source: &str,
        theme: DiagramTheme,
    ) -> CachedDiagram {
        let key = (normalize_mermaid_source(source), theme);
        if let Some(hit) = cache.get(&key) {
            return hit;
        }
        let val = match validate_and_render(source, theme) {
            Ok(rendered) => CachedDiagram::Ready {
                image: Arc::new(Image::from_bytes(ImageFormat::Png, rendered.png_bytes)),
                width: rendered.width,
                height: rendered.height,
            },
            Err(err) => CachedDiagram::Error(err),
        };
        cache.insert(key, val.clone());
        val
    }

    #[test]
    fn plain_markdown_without_mermaid_returns_single_segment() {
        let md = "# Heading\n\nSome **bold** text and `inline` code.\n\n```rust\nfn main() {}\n```";
        let segments = split_markdown_segments(md);
        assert_eq!(segments, vec![MarkdownSegment::Markdown(md.to_string())]);
    }

    #[test]
    fn splits_single_and_multiple_mermaid_blocks_interleaved_with_prose() {
        let md = "\
# Architecture

Intro paragraph.

```mermaid
flowchart LR
    A --> B
```

Middle paragraph.

```mermaid
sequenceDiagram
    Alice->>Bob: Hi
```

Closing paragraph.";

        let segments = split_markdown_segments(md);
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Markdown("# Architecture\n\nIntro paragraph.\n".to_string()),
                MarkdownSegment::Mermaid("flowchart LR\n    A --> B".to_string()),
                MarkdownSegment::Markdown("Middle paragraph.\n".to_string()),
                MarkdownSegment::Mermaid("sequenceDiagram\n    Alice->>Bob: Hi".to_string()),
                MarkdownSegment::Markdown("Closing paragraph.".to_string()),
            ]
        );
    }

    #[test]
    fn handles_case_insensitive_info_strings_and_tilde_fences() {
        let md = "\
~~~Mermaid
flowchart TD
    X --> Y
~~~

```MERMAID title=\"System\"
graph LR
    P --> Q
```";

        let segments = split_markdown_segments(md);
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Mermaid("flowchart TD\n    X --> Y".to_string()),
                MarkdownSegment::Mermaid("graph LR\n    P --> Q".to_string()),
            ]
        );
    }

    #[test]
    fn preserves_nested_mermaid_fence_inside_outer_markdown_fence() {
        let md = "\
Example:

````markdown
```mermaid
flowchart LR
    A --> B
```
````

Done.";

        let segments = split_markdown_segments(md);
        assert_eq!(segments, vec![MarkdownSegment::Markdown(md.to_string())]);
    }

    #[test]
    fn preserves_unclosed_mermaid_fence_as_markdown() {
        let md = "# Streaming\n\n```mermaid\nflowchart TD\n    A --> B";
        let segments = split_markdown_segments(md);
        assert_eq!(segments, vec![MarkdownSegment::Markdown(md.to_string())]);
    }

    #[test]
    fn handles_crlf_line_endings_cleanly() {
        let md = "Before\r\n\r\n```mermaid\r\nflowchart LR\r\n    A --> B\r\n```\r\n\r\nAfter";
        let segments = split_markdown_segments(md);
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Markdown("Before\n".to_string()),
                MarkdownSegment::Mermaid("flowchart LR\n    A --> B".to_string()),
                MarkdownSegment::Markdown("After".to_string()),
            ]
        );
    }

    #[test]
    fn cache_stores_light_and_dark_independently_and_evicts_oldest() {
        let mut cache = MermaidRenderCache::new(2);
        let src = "flowchart LR\n    A --> B";

        let light = render_into_cache(&mut cache, src, DiagramTheme::Light);
        assert!(matches!(light, CachedDiagram::Ready { width, height, .. } if width > 0.0 && height > 0.0));

        let dark = render_into_cache(&mut cache, src, DiagramTheme::Dark);
        assert!(matches!(dark, CachedDiagram::Ready { width, height, .. } if width > 0.0 && height > 0.0));

        assert_eq!(cache.map.len(), 2);

        // Inserting a 3rd distinct key should evict the oldest entry (src, Light).
        let other = "flowchart TD\n    C --> D";
        let _ = render_into_cache(&mut cache, other, DiagramTheme::Light);
        assert_eq!(cache.map.len(), 2);
        assert!(cache.get(&(normalize_mermaid_source(src), DiagramTheme::Light)).is_none());
        assert!(cache.get(&(normalize_mermaid_source(src), DiagramTheme::Dark)).is_some());
        assert!(cache.get(&(normalize_mermaid_source(other), DiagramTheme::Light)).is_some());
    }

    #[test]
    fn invalid_mermaid_caches_error_state() {
        let mut cache = MermaidRenderCache::new(4);
        let broken = "not a valid diagram %%%%";
        let outcome = render_into_cache(&mut cache, broken, DiagramTheme::Light);
        assert!(matches!(outcome, CachedDiagram::Error(msg) if !msg.is_empty()));
    }
}
