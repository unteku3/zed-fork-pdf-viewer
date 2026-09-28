mod context;
mod document;
mod pdf_viewer_settings;
mod viewer;
use anyhow::{Context, Result};
use context::PdfSelection;
#[cfg(test)]
use context::{context_pages, page_list};
use document::PdfDocument;
#[cfg(test)]
use pdf_viewer_settings::PdfSendMode;
use pdf_viewer_settings::PdfViewerSettings;
use std::{collections::HashMap, io};
use viewer_protocol::*;
fn render_clipboard_region(doc: &PdfDocument, page: usize, rect: kurbo::Rect) -> Result<Vec<u8>> {
    doc.render_region(page, rect, 2.0)
        .context("Could not render selected region")?
        .to_png()
}
fn main() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut views = HashMap::<u64, viewer::Viewer>::new();
    let mut hello = false;
    loop {
        let bytes = match read_frame(&mut input) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        let req: Request = serde_json::from_slice(&bytes)?;
        let mut response = Response {
            id: req.id,
            ..Default::default()
        };
        let mut blobs = vec![];
        let result = (|| -> Result<()> {
            if let Command::Hello { api_version } = req.command {
                anyhow::ensure!(
                    api_version == API_VERSION,
                    "Incompatible graphic extension API"
                );
                hello = true;
                return Ok(());
            }
            anyhow::ensure!(hello, "Handshake required");
            match req.command {
                Command::Open {
                    path,
                    settings,
                    state,
                } => {
                    views.insert(req.view, viewer::Viewer::open(path, settings, state)?);
                }
                Command::Update {
                    width,
                    height,
                    scale,
                    event,
                    settings,
                } => {
                    for (id, view) in &mut views {
                        if *id != req.view {
                            view.clear_cache();
                        }
                    }
                    let v = views.get_mut(&req.view).context("View is closed")?;
                    v.update(
                        width,
                        height,
                        scale,
                        event,
                        settings,
                        &mut response,
                        &mut blobs,
                    )?;
                }
                Command::Context {
                    selection,
                    images,
                    first: _,
                } => {
                    let v = views.get(&req.view).context("View is closed")?;
                    let sel = v
                        .selections
                        .get(&selection)
                        .context("Selection expired; select again")?;
                    let text = sel.context_page_text()?;
                    let names = if images {
                        blobs = sel.image_bytes()?;
                        sel.image_names()
                    } else {
                        vec![]
                    };
                    response.context = Some(ContextPayload {
                        text,
                        image_names: names,
                    });
                }
                Command::Close => {
                    views.remove(&req.view);
                }
                Command::Hello { .. } => unreachable!(),
            }
            Ok(())
        })();
        if let Err(e) = result {
            response.error = Some(format!("{e:#}"));
            blobs.clear();
        }
        response.blobs = blobs.len();
        write_frame(&mut output, &serde_json::to_vec(&response)?)?;
        for b in blobs {
            write_frame(&mut output, &b)?;
        }
    }
    Ok(())
}
