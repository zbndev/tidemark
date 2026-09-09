//! `tidemarkctl plugin`: the authoring and administration surface for user-installed
//! providers.
//!
//! Everything here reads local files and formats what comes back. It does not parse a
//! plugin, run Lua or sanitize an SVG — there is one parser and one runtime, both in the
//! daemon, and a second copy in the CLI would be a second set of verdicts to keep in step.
//! That is also why `render` sends both files over the bus rather than transforming
//! anything locally.
//!
//! The one thing it decides by itself is whether a path it was given can be read at all: a
//! file the shell mistyped is the user's problem before it is the daemon's, and forwarding
//! nothing would turn a typo into a parse error about an empty file.

use std::path::Path;

use tidemark_ipc::DaemonProxy;
use tidemark_types::{Metric, PluginInfo, Presentation, ProviderDefinition, Widget};

use crate::cli::{PluginCommand, PluginFormat};
use crate::exit::{Exit, Failure};

pub async fn run(proxy: &DaemonProxy<'_>, command: PluginCommand) -> Result<Exit, Failure> {
    match command {
        PluginCommand::Validate { file, format } => {
            let info = proxy.inspect_plugin(read(&file)?).await?;
            print_info(&info, format, "would install")?;
        }
        PluginCommand::Install { file, format } => {
            let info = proxy.install_plugin(read(&file)?).await?;
            print_info(&info, format, "installed")?;
        }
        PluginCommand::Render {
            file,
            response,
            format,
        } => {
            let rendered = proxy.render_plugin(read(&file)?, read(&response)?).await?;
            print_render(&rendered, format)?;
        }
        PluginCommand::List { format } => {
            let installed: Vec<PluginInfo> = proxy
                .list_providers()
                .await?
                .into_iter()
                // The daemon publishes one catalog, and a plugin's entry is the one that
                // carries its metadata. Asking for a second, plugin-only listing would be
                // a second answer to keep in step with the first.
                .filter_map(|definition: ProviderDefinition| definition.plugin)
                .collect();
            print_list(&installed, format)?;
        }
        PluginCommand::Remove { provider } => proxy.remove_plugin(&provider).await?,
        PluginCommand::Endpoint {
            provider,
            url,
            account,
            allow_insecure_http,
        } => {
            proxy
                .set_plugin_endpoint(&provider, &account, &url, allow_insecure_http)
                .await?;
        }
    }
    Ok(Exit::Ok)
}

/// The file's bytes, or a usage failure naming the path the user typed.
///
/// `Usage` rather than `Unavailable`: nothing is wrong with the daemon, and a script that
/// retries on `Unavailable` would retry a path that will never appear.
fn read(path: &Path) -> Result<Vec<u8>, Failure> {
    std::fs::read(path)
        .map_err(|error| Failure::usage(format!("cannot read {}: {error}", path.display())))
}

/// What a definition declares, in the shape a person reads before trusting it.
///
/// The header and prefix are the point of the preview: they say exactly where this file
/// will put the key the user is about to give it.
fn print_info(info: &PluginInfo, format: PluginFormat, verb: &str) -> Result<(), Failure> {
    match format {
        PluginFormat::Json => println!("{}", json(info)?),
        PluginFormat::Text => {
            println!("{verb:<12} {}", info.id);
            println!("name         {}", info.name);
            println!("version      {}", info.plugin_version);
            println!("method       {}", info.method);
            println!("key header   {}", info.api_key_header);
            println!(
                "key prefix   {}",
                if info.api_key_prefix.is_empty() {
                    "(none)"
                } else {
                    &info.api_key_prefix
                }
            );
            println!("mark         {}", if info.has_mark { "yes" } else { "no" });
        }
    }
    Ok(())
}

fn print_list(installed: &[PluginInfo], format: PluginFormat) -> Result<(), Failure> {
    match format {
        PluginFormat::Json => println!("{}", json(installed)?),
        PluginFormat::Text => {
            for info in installed {
                println!(
                    "{:<24} {:<24} {:<10} {}",
                    info.id, info.name, info.plugin_version, info.api_key_header
                );
            }
        }
    }
    Ok(())
}

/// A rendered fixture, in the order the daemon returned it.
///
/// The card comes first and keeps its order, because that order *is* the answer an author
/// is looking for: it is what the grid will draw without opening the GUI.
fn print_render(rendered: &Presentation, format: PluginFormat) -> Result<(), Failure> {
    match format {
        PluginFormat::Json => println!("{}", json(rendered)?),
        PluginFormat::Text => {
            println!("card");
            for widget in &rendered.card {
                println!("  {}", widget_line(widget, rendered));
            }
            for section in &rendered.details {
                println!("{}", section.title);
                for widget in &section.items {
                    println!("  {}", widget_line(widget, rendered));
                }
            }
            println!("metrics");
            for metric in &rendered.metrics {
                println!("  {}", metric_line(metric));
            }
        }
    }
    Ok(())
}

/// One widget with the metric it points at, so a placement that names nothing is visible
/// as such rather than silently drawing empty.
fn widget_line(widget: &Widget, rendered: &Presentation) -> String {
    let title = rendered
        .metric(&widget.metric)
        .map_or("(no such metric)", |metric| metric.title.as_str());
    match widget.field.as_deref() {
        Some(field) => format!(
            "{:<8} {:<20} {:<12} {title}",
            widget.kind, widget.metric, field
        ),
        None => format!(
            "{:<8} {:<20} {:<12} {title}",
            widget.kind, widget.metric, ""
        ),
    }
}

/// One metric's numbers, absent ones left blank rather than printed as zero.
fn metric_line(metric: &Metric) -> String {
    let number = |value: Option<f64>| value.map_or_else(String::new, |value| format!("{value}"));
    format!(
        "{:<20} {:<24} {:>10} {:>10} {}",
        metric.id,
        metric.title,
        number(metric.value),
        number(metric.maximum),
        metric.unit.as_deref().unwrap_or("")
    )
}

/// The published dictionary without its D-Bus envelope, the same peeling `usage --format
/// json` does.
fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, Failure> {
    let document = serde_json::to_value(value)?;
    Ok(serde_json::to_string_pretty(&crate::format::payload(
        document,
    ))?)
}
