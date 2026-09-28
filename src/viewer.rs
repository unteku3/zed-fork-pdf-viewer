use crate::{
    PdfDocument, PdfSelection, PdfViewerSettings,
    document::{PageTextLayer, render_scale},
    render_clipboard_region,
};
use anyhow::Result;
use kurbo::{Point, Rect};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};
use viewer_protocol::*;
struct Cached {
    scale: f32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}
pub struct Viewer {
    document: Arc<PdfDocument>,
    path: PathBuf,
    settings: PdfViewerSettings,
    width: f32,
    height: f32,
    scale: f32,
    zoom: f32,
    top: f32,
    left: f32,
    initialized: bool,
    sizes: Vec<(f32, f32)>,
    cache: HashMap<usize, Cached>,
    text: HashMap<usize, PageTextLayer>,
    drag: Option<(usize, Point, usize, bool)>,
    selected: Option<(usize, Rect, String, Vec<Rect>)>,
    serial: u64,
    sent: std::collections::BTreeSet<String>,
    failed: std::collections::BTreeSet<usize>,
    pub selections: BTreeMap<u64, PdfSelection>,
}
impl Viewer {
    pub fn open(
        path: String,
        settings: serde_json::Value,
        state: serde_json::Value,
    ) -> Result<Self> {
        let document =
            Arc::new(PdfDocument::load(std::fs::read(&path)?).map_err(anyhow::Error::msg)?);
        let sizes = (0..document.page_count())
            .map(|i| document.page_size(i).unwrap_or((612., 792.)))
            .collect();
        let mut result = Self {
            document,
            path: path.into(),
            settings: PdfViewerSettings::from_value(settings)?,
            width: 800.,
            height: 600.,
            scale: 1.,
            zoom: 1.,
            top: 0.,
            left: 0.,
            initialized: false,
            sizes,
            cache: HashMap::new(),
            text: HashMap::new(),
            drag: None,
            selected: None,
            serial: 0,
            sent: Default::default(),
            failed: Default::default(),
            selections: BTreeMap::new(),
        };
        if let Some(z) = state
            .get("zoom")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
        {
            result.zoom = (z as f32).clamp(0.2, 8.);
            result.initialized = true;
        }
        result.top = state.get("top").and_then(|v| v.as_f64()).unwrap_or(0.) as f32;
        result.left = state.get("left").and_then(|v| v.as_f64()).unwrap_or(0.) as f32;
        Ok(result)
    }
    fn page_top(&self, p: usize) -> f32 {
        16. + self.sizes[..p]
            .iter()
            .map(|s| s.1 * self.zoom + 12.)
            .sum::<f32>()
    }
    fn page_left(&self, p: usize) -> f32 {
        ((self.width - self.sizes[p].0 * self.zoom) / 2.).max(0.) - self.left
    }
    fn fit(&self) -> f32 {
        ((self.width - 32.) / self.sizes.iter().map(|s| s.0).fold(1., f32::max)).clamp(0.2, 8.)
    }
    fn clamp(&mut self) {
        self.top = self.top.clamp(
            0.,
            (self.page_top(self.sizes.len()) + 16. - self.height + 40.).max(0.),
        );
        self.left = self.left.clamp(
            0.,
            (self
                .sizes
                .iter()
                .map(|s| s.0 * self.zoom)
                .fold(0., f32::max)
                - self.width)
                .max(0.),
        );
    }
    fn set_zoom(&mut self, z: f32) {
        let z = z.clamp(0.2, 8.);
        let ratio = z / self.zoom;
        self.top = (self.top + self.height / 2.) * ratio - self.height / 2.;
        self.left *= ratio;
        self.zoom = z;
        self.clamp();
    }
    fn locate(&self, x: f32, y: f32) -> Option<(usize, Point)> {
        if y < 40. {
            return None;
        }
        let local_y = y - 40. + self.top;
        for (p, (w, h)) in self.sizes.iter().enumerate() {
            let pt = Point::new(
                ((x - self.page_left(p)) / self.zoom) as f64,
                ((local_y - self.page_top(p)) / self.zoom) as f64,
            );
            if Rect::new(0., 0., *w as f64, *h as f64).contains(pt) {
                return Some((p, pt));
            }
        }
        None
    }
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.text.clear();
    }
    pub fn update(
        &mut self,
        width: f32,
        height: f32,
        scale: f32,
        event: Event,
        settings: serde_json::Value,
        out: &mut Response,
        blobs: &mut Vec<Vec<u8>>,
    ) -> Result<()> {
        anyhow::ensure!(
            width.is_finite()
                && height.is_finite()
                && scale.is_finite()
                && width > 0.
                && height > 0.,
            "Invalid viewport"
        );
        self.width = width.min(32768.);
        self.height = height.min(32768.);
        self.scale = scale.clamp(1., 4.);
        let background = settings
            .get("_background")
            .and_then(|v| v.as_u64())
            .unwrap_or(0x242424ff) as u32;
        self.settings = PdfViewerSettings::from_value(settings)?;
        if !self.initialized {
            self.zoom = self.fit();
            self.initialized = true;
        }
        let force_images = matches!(&event, Event::Refresh);
        match event {
            Event::Action(action) => match action.as_str() {
                "zoom_in" => self.set_zoom(self.zoom * 1.15),
                "zoom_out" => self.set_zoom(self.zoom / 1.15),
                "fit" => self.set_zoom(self.fit()),
                "copy" => {
                    if let Some((_, _, text, _)) = &self.selected {
                        out.clipboard = Some(Clipboard::Text(text.clone()));
                    }
                }
                _ => {}
            },
            Event::Scroll { x, y } => {
                if x.is_finite() && y.is_finite() {
                    self.top -= y;
                    self.left -= x;
                }
                self.clamp();
            }
            Event::Pointer { kind, x, y, alt } => {
                if kind == "down" && y >= 40. {
                    self.selected = None;
                    self.drag = self.locate(x, y).map(|(p, point)| {
                        if !self.text.contains_key(&p) {
                            if let Some(t) = self.document.text_layer(p) {
                                self.text.insert(p, t);
                            }
                        }
                        let glyph = self.text.get(&p).map_or(0, |t| t.index_at(point));
                        (p, point, glyph, alt)
                    });
                }
                if kind == "move" || kind == "up" {
                    if let Some((p, anchor, glyph, region)) = self.drag {
                        let (w, h) = self.sizes[p];
                        let point = Point::new(
                            (((x - self.page_left(p)) / self.zoom) as f64).clamp(0., w as f64),
                            (((y - 40. + self.top - self.page_top(p)) / self.zoom) as f64)
                                .clamp(0., h as f64),
                        );
                        if region {
                            let r = Rect::from_points(anchor, point);
                            let text = self
                                .text
                                .get(&p)
                                .map_or(String::new(), |t| t.text_in_region(r));
                            self.selected = Some((p, r, text, vec![r]));
                        } else if let Some(t) = self.text.get(&p) {
                            let g = t.index_at(point);
                            let range = glyph.min(g)..glyph.max(g);
                            let rects = t.selection_rects(range.clone());
                            if let Some(r) = rects.iter().copied().reduce(|a, b| a.union(b)) {
                                self.selected = Some((p, r, t.selected_text(range), rects));
                            }
                        }
                        if kind == "up" && region && self.settings.auto_copy_region {
                            if let Some((p, r, _, _)) = &self.selected {
                                if !r.is_zero_area() {
                                    let b = render_clipboard_region(&self.document, *p, *r)?;
                                    out.clipboard = Some(Clipboard::Png { blob: blobs.len() });
                                    blobs.push(b);
                                }
                            }
                        }
                    }
                    if kind == "up" {
                        self.drag = None;
                    }
                }
            }
            _ => {}
        }
        let visible: Vec<_> = (0..self.sizes.len())
            .filter(|&p| {
                let y = self.page_top(p) - self.top;
                y + self.sizes[p].1 * self.zoom >= 0. && y < self.height - 40.
            })
            .collect();
        let start = visible.first().copied().unwrap_or(0).saturating_sub(2);
        let end = (visible.last().copied().unwrap_or(0) + 3).min(self.sizes.len());
        self.cache.retain(|p, _| (*p >= start) && (*p < end));
        self.text.retain(|p, _| (*p >= start) && (*p < end));
        let mut scene = Scene::default();
        let mut node = |id: &str, bounds, kind, tooltip| {
            scene.nodes.push(Node {
                id: id.into(),
                bounds,
                kind,
                tooltip,
            })
        };
        node(
            "page-count",
            [8., 6., 120., 28.],
            NodeKind::Text(format!(
                "{} / {}",
                visible.first().map_or(0, |p| p + 1),
                self.sizes.len()
            )),
            None,
        );
        for (id, label, x, w) in [
            ("zoom_out", "−", 132., 32.),
            ("zoom_in", "+", 242., 32.),
            ("fit", "Fit width", 282., 90.),
            ("copy", "Copy", 382., 60.),
        ] {
            node(
                id,
                [x, 4., w, 30.],
                NodeKind::Button {
                    label: label.into(),
                    action: id.into(),
                },
                None,
            );
        }
        node(
            "zoom",
            [174., 6., 65., 28.],
            NodeKind::Text(format!("{:.0}%", self.zoom * 100.)),
            None,
        );
        node("hint",[452.,6.,24.,28.],NodeKind::Text("ⓘ".into()),Some("Select text or Option-drag an area; Return adds it to the agent. Configure pdf_viewer in settings.json.".into()));
        let mut rendered = 0;
        let mut used = self.cache.values().map(|c| c.pixels.len()).sum::<usize>();
        let budget = CACHE_BUDGET / 2;
        for &p in &visible {
            let (w, h) = self.sizes[p];
            let target = render_scale(
                w,
                h,
                (self.zoom * self.scale)
                    .min(((budget / visible.len().max(1)) as f32 / (4. * w * h)).sqrt()),
            )
            .unwrap_or(1.);
            if self
                .cache
                .get(&p)
                .is_some_and(|c| c.pixels.len() > budget / visible.len().max(1))
            {
                if let Some(old) = self.cache.remove(&p) {
                    used -= old.pixels.len();
                }
            }
            if !self.failed.contains(&p)
                && self.cache.get(&p).is_none_or(|c| c.scale + 0.01 < target)
                && rendered < 2
            {
                let requested = if self.cache.contains_key(&p) {
                    target
                } else {
                    target.min((600_000. / (w * h)).sqrt())
                };
                if let Some(r) = self.document.render_page(p, requested) {
                    let len = r.bgra.len();
                    if let Some(old) = self.cache.remove(&p) {
                        used -= old.pixels.len();
                    }
                    while used + len > budget && !self.cache.is_empty() {
                        let k = *self.cache.keys().next().unwrap();
                        used -= self.cache.remove(&k).unwrap().pixels.len();
                    }
                    used += len;
                    self.cache.insert(
                        p,
                        Cached {
                            scale: requested,
                            width: r.width,
                            height: r.height,
                            pixels: r.bgra,
                        },
                    );
                    rendered += 1;
                } else {
                    self.failed.insert(p);
                }
            }
            let x = self.page_left(p);
            let y = self.page_top(p) - self.top + 40.;
            // All document nodes are clipped below the toolbar by the host via their container bounds.
            node(
                &format!("paper-{p}"),
                [x, y, w * self.zoom, h * self.zoom],
                NodeKind::Rect { color: 0xffffffff },
                None,
            );
            if let Some(c) = self.cache.get(&p) {
                let key = format!("{p}-{}-{}", c.width, c.height);
                let blob = if force_images || !self.sent.contains(&key) {
                    let n = blobs.len();
                    blobs.push(c.pixels.clone());
                    Some(n)
                } else {
                    None
                };
                node(
                    &format!("image-{p}"),
                    [x, y, w * self.zoom, h * self.zoom],
                    NodeKind::Image {
                        key,
                        blob,
                        width: c.width,
                        height: c.height,
                    },
                    None,
                );
                scene.pending |= c.scale + 0.01 < target;
            } else if self.failed.contains(&p) {
                node(
                    &format!("failed-{p}"),
                    [x + 12., y + 12., 240., 28.],
                    NodeKind::Text("Could not render page".into()),
                    None,
                );
            } else {
                scene.pending = true;
            }
            if let Some((selected, _, _, rects)) = &self.selected {
                if *selected == p {
                    for (i, r) in rects.iter().enumerate() {
                        node(
                            &format!("selection-{p}-{i}"),
                            [
                                x + r.x0 as f32 * self.zoom,
                                y + r.y0 as f32 * self.zoom,
                                r.width() as f32 * self.zoom,
                                r.height() as f32 * self.zoom,
                            ],
                            NodeKind::Rect { color: 0x4488ff66 },
                            None,
                        );
                    }
                }
            }
        }
        // Prefetch nearby previews without evicting visible images or exceeding the shared raster budget.
        for p in start..end {
            if visible.contains(&p) || self.cache.contains_key(&p) || self.failed.contains(&p) {
                continue;
            }
            let (w, h) = self.sizes[p];
            let scale = (600_000. / (w * h)).sqrt().min(self.zoom * self.scale);
            let estimated = (w * scale).ceil() as usize * (h * scale).ceil() as usize * 4;
            if used + estimated > budget {
                continue;
            }
            if rendered >= 2 {
                scene.pending = true;
                break;
            }
            if let Some(r) = self.document.render_page(p, scale) {
                used += r.bgra.len();
                self.cache.insert(
                    p,
                    Cached {
                        scale,
                        width: r.width,
                        height: r.height,
                        pixels: r.bgra,
                    },
                );
                rendered += 1;
            } else {
                self.failed.insert(p);
            }
        }
        // Reorder toolbar above page content, so scrolling pages never cover controls.
        let toolbar = scene.nodes.drain(..7).collect::<Vec<_>>();
        scene.nodes.push(Node {
            id: "toolbar-background".into(),
            bounds: [0., 0., self.width, 40.],
            kind: NodeKind::Rect { color: background },
            tooltip: None,
        });
        scene.nodes.extend(toolbar);
        if let Some((p, r, text, _)) = &self.selected {
            if !r.is_zero_area() {
                self.serial += 1;
                let selection = PdfSelection {
                    text: text.clone(),
                    page: *p,
                    page_count: self.sizes.len(),
                    path: self.path.clone(),
                    region: Some(*r),
                    settings: self.settings.clone(),
                    document: self.document.clone(),
                };
                scene.selection = Some(Selection {
                    token: self.serial,
                    path: self.path.display().to_string(),
                    text: text.clone(),
                    first: selection.context_text_for_agent(true, true),
                    repeat: selection.context_text_for_agent(true, false),
                    first_no_images: selection.context_text_for_agent(false, true),
                    repeat_no_images: selection.context_text_for_agent(false, false),
                    image_names: selection.image_names(),
                    terminal_first: if self.drag.is_none() {
                        selection.context_text_for_terminal(true)
                    } else {
                        selection.context_text_for_agent(false, true)
                    },
                    terminal_repeat: if self.drag.is_none() {
                        selection.context_text_for_terminal(false)
                    } else {
                        selection.context_text_for_agent(false, false)
                    },
                });
                self.selections.insert(self.serial, selection);
                while self.selections.len() > 64 {
                    self.selections.pop_first();
                }
            }
        }
        scene.state = serde_json::json!({"zoom":self.zoom,"top":self.top,"left":self.left});
        scene.keys = [
            ("cmd-c", "copy"),
            ("cmd-=", "zoom_in"),
            ("cmd-+", "zoom_in"),
            ("cmd--", "zoom_out"),
            ("cmd-0", "fit"),
            ("enter", "host:add_selection"),
            ("cmd->", "host:add_selection"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        self.sent = scene
            .nodes
            .iter()
            .filter_map(|n| {
                if let NodeKind::Image { key, .. } = &n.kind {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();
        out.scene = Some(scene);
        Ok(())
    }
}
