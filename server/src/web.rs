use crate::{
    api::AppState,
    errors::ApiError,
    programs::{self, ProgramSummary},
};
use axum::{
    extract::{Multipart, State as AxumState},
    response::Html,
};
use maud::{DOCTYPE, Markup, html};
use std::sync::Arc;

pub(crate) async fn index(
    AxumState(state): AxumState<Arc<AppState>>,
) -> Result<Html<String>, ApiError> {
    let programs = programs::list_programs(&state.data_dir)?;
    let markup = render_index(&programs, None, None);
    Ok(Html(markup.into_string()))
}

pub(crate) async fn upload(
    AxumState(state): AxumState<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Html<String>, ApiError> {
    let mut program_id: Option<String> = None;
    let mut wasm_bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(ApiError::internal_anyhow)?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "program_id" {
            let value = field.text().await.map_err(ApiError::internal_anyhow)?;
            program_id = Some(value.trim().to_string());
        } else if name == "program" {
            let bytes = field.bytes().await.map_err(ApiError::internal_anyhow)?;
            wasm_bytes = Some(bytes.to_vec());
        }
    }

    let program_id = program_id
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("program_id is required"))?;
    if !is_valid_program_id(&program_id) {
        return Err(ApiError::bad_request(
            "program_id must be alphanumeric with - or _ only",
        ));
    }

    let wasm_bytes = wasm_bytes.ok_or_else(|| ApiError::bad_request("program file is required"))?;
    if wasm_bytes.is_empty() {
        return Err(ApiError::bad_request("program file is empty"));
    }

    let program_dir = state.data_dir.join(&program_id);
    if program_dir.exists() {
        return Err(ApiError::bad_request("program already exists"));
    }

    std::fs::create_dir_all(&program_dir)
        .map_err(|e| ApiError::internal(format!("failed to create program dir: {e}")))?;
    let wasm_path = programs::program_wasm_path(&program_dir);
    std::fs::write(&wasm_path, &wasm_bytes)
        .map_err(|e| ApiError::internal(format!("failed to write program.wasm: {e}")))?;

    let api_key = programs::generate_api_key();
    programs::write_api_key(&program_dir, &api_key)?;
    state
        .api_keys
        .write()
        .await
        .insert(program_id.clone(), api_key.clone());

    let programs = programs::list_programs(&state.data_dir)?;
    let markup = render_index(&programs, Some(&api_key), Some(&program_id));
    Ok(Html(markup.into_string()))
}

fn is_valid_program_id(program_id: &str) -> bool {
    program_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn render_index(
    programs: &[ProgramSummary],
    api_key: Option<&str>,
    program_id: Option<&str>,
) -> Markup {
    let is_empty = programs.is_empty();
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "ribolita" }
                script type="module" src="https://cdn.jsdelivr.net/npm/datastar@1.0.0/dist/datastar.min.js" {}
                script src="https://cdn.tailwindcss.com" {}
            }
            body data-star="app" data-signals="{\"selectedProgram\": null}" class="bg-gradient-to-b from-stone-100 via-amber-50 to-stone-200 text-stone-900" {
                main class="mx-auto max-w-5xl px-6 py-12" {
                    header class="flex flex-col gap-4 md:flex-row md:items-end md:justify-between" {
                        div {
                            h1 class="text-3xl mb-8 font-semibold tracking-tight md:text-4xl" { "ribolita programs" }
                        }
                    }
                    div class="grid gap-6 md:grid-cols-2" {
                        section class="rounded-2xl border border-stone-200 bg-white/80 p-6 shadow-lg shadow-stone-300/40" {
                            h2 class="text-base font-semibold text-stone-800" { "Programs" }
                            ul class="mt-4 grid gap-3" {
                                @for program in programs {
                                    li class="flex items-center justify-between rounded-xl border border-stone-200 bg-white px-4 py-3 text-sm" {
                                        button class="text-left text-stone-700 hover:text-stone-900" type="button" data-program={(format!(r#"{{\"id\":\"{}\"}}"#, program.id))} data-on-click="selectedProgram = JSON.parse($el.dataset.program)" { (program.id) }
                                        code class="rounded-md bg-emerald-50 px-2 py-1 font-mono text-xs text-emerald-700" { (program.id) }
                                    }
                                }
                                @if is_empty {
                                    li class="rounded-xl border border-dashed border-stone-200 bg-white px-4 py-6 text-center text-sm text-stone-500" { "No programs found" }
                                }
                            }
                            div class="mt-4 rounded-xl border border-dashed border-stone-300 bg-stone-50 px-4 py-4" {
                                p class="text-xs font-semibold uppercase tracking-wide text-stone-500" { "Program state" }
                                pre class="mt-2 text-xs text-stone-700 whitespace-pre-wrap" data-text="selectedProgram ? JSON.stringify(selectedProgram, null, 2) : 'Click a program to see its state.'" {}
                            }
                        }
                        section class="rounded-2xl border border-stone-200 bg-white/80 p-6 shadow-lg shadow-stone-300/40" {
                            h2 class="text-base font-semibold text-stone-800" { "Upload new program" }
                            form class="mt-4 grid gap-4" method="post" action="/upload" enctype="multipart/form-data" data-star="submit" data-target="#upload-result" {
                                label class="text-xs font-semibold uppercase tracking-wide text-stone-500" for="program_id" { "Program id" }
                                input class="w-full rounded-xl border border-stone-200 bg-white px-3 py-2 text-sm" type="text" name="program_id" id="program_id" placeholder="e.g. telemetry" required="";
                                label class="text-xs font-semibold uppercase tracking-wide text-stone-500" for="program" { "program.wasm file" }
                                input class="w-full rounded-xl border border-stone-200 bg-white px-3 py-2 text-sm" type="file" name="program" id="program" accept=".wasm" required="";
                                button class="rounded-xl bg-emerald-700 px-4 py-2 text-sm font-semibold text-white shadow-sm transition hover:bg-emerald-800" type="submit" { "Upload program" }
                            }
                            div class="mt-5" id="upload-result" {
                                @if let Some(api_key) = api_key {
                                    div class="grid gap-2 rounded-2xl border border-dashed border-emerald-600/40 bg-emerald-50 px-4 py-4 text-sm" {
                                        p class="font-semibold text-emerald-800" { "API key for " (program_id.unwrap_or("(unknown)")) ":" }
                                        code class="rounded-md bg-white px-2 py-1 font-mono text-xs text-emerald-800 truncate" { (api_key) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
