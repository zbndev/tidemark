//! The desktop's appearance, read from the XDG settings portal: whether the user prefers a
//! dark style, and their accent colour. libadwaita follows both on its own; Slint follows
//! neither, so this asks the same portal and keeps asking while the program runs.

use std::future::poll_fn;
use std::pin::pin;

use zbus::export::futures_core::Stream;
use zbus::zvariant::{OwnedValue, Value};

use crate::window::Appearance;

const NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";
const ACCENT_COLOR: &str = "accent-color";

/// Reports the current appearance, then every change to it. Silently gives up on a desktop
/// without the portal: the window keeps libadwaita's defaults.
pub fn watch(on: impl Fn(Appearance) + 'static) {
    let spawned = slint::spawn_local(async move {
        if let Err(error) = serve(&on).await {
            tracing::info!(%error, "no settings portal; keeping the default appearance");
        }
    });
    if let Err(error) = spawned {
        tracing::error!(%error, "the event loop refused the portal watcher");
    }
}

async fn serve(on: &impl Fn(Appearance)) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .await?;
    let mut changes = pin!(proxy.receive_signal("SettingChanged").await?);

    for key in [COLOR_SCHEME, ACCENT_COLOR] {
        match proxy
            .call::<_, _, OwnedValue>("ReadOne", &(NAMESPACE, key))
            .await
        {
            Ok(value) => {
                if let Some(appearance) = parse(key, &value) {
                    on(appearance);
                }
            }
            Err(error) => tracing::debug!(%error, key, "the portal did not answer"),
        }
    }

    loop {
        let message = poll_fn(|context| changes.as_mut().poll_next(context)).await;
        let Some(message) = message else {
            return Ok(());
        };
        let Ok((namespace, key, value)) =
            message.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == NAMESPACE
            && let Some(appearance) = parse(&key, &value)
        {
            on(appearance);
        }
    }
}

fn parse(key: &str, value: &Value<'_>) -> Option<Appearance> {
    // `ReadOne` wraps the setting in a variant; `Read`-era portals wrapped it twice.
    if let Value::Value(inner) = value {
        return parse(key, inner);
    }
    match key {
        // 0: no preference, 1: prefer dark, 2: prefer light.
        COLOR_SCHEME => match value {
            Value::U32(scheme) => Some(Appearance::Dark(*scheme == 1)),
            _ => None,
        },
        ACCENT_COLOR => {
            let (red, green, blue) = <(f64, f64, f64)>::try_from(value.try_clone().ok()?).ok()?;
            let usable = [red, green, blue]
                .iter()
                .all(|channel| (0.0..=1.0).contains(channel));
            Some(Appearance::Accent(usable.then_some([red, green, blue])))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dark_preference_is_one_and_only_one() {
        assert_eq!(
            parse(COLOR_SCHEME, &Value::U32(1)),
            Some(Appearance::Dark(true))
        );
        assert_eq!(
            parse(COLOR_SCHEME, &Value::U32(2)),
            Some(Appearance::Dark(false))
        );
        assert_eq!(
            parse(COLOR_SCHEME, &Value::U32(0)),
            Some(Appearance::Dark(false))
        );
    }

    #[test]
    fn an_accent_outside_the_unit_cube_means_no_accent() {
        let accent = Value::from((0.2_f64, 0.5_f64, 0.9_f64));
        assert_eq!(
            parse(ACCENT_COLOR, &accent),
            Some(Appearance::Accent(Some([0.2, 0.5, 0.9])))
        );
        let unset = Value::from((-1.0_f64, -1.0_f64, -1.0_f64));
        assert_eq!(parse(ACCENT_COLOR, &unset), Some(Appearance::Accent(None)));
    }
}
