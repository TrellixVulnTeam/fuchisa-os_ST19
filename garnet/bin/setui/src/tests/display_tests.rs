// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
use {
    crate::agent::restore_agent,
    crate::config::base::ControllerFlag,
    crate::handler::device_storage::testing::*,
    crate::switchboard::base::{DisplayInfo, LowLightMode, SettingType, Theme},
    crate::tests::fakes::brightness_service::BrightnessService,
    crate::tests::fakes::service_registry::ServiceRegistry,
    crate::tests::test_failure_utils::create_test_env_with_failures,
    crate::EnvironmentBuilder,
    anyhow::format_err,
    fidl::endpoints::{ServerEnd, ServiceMarker},
    fidl::Error::ClientChannelClosed,
    fidl_fuchsia_settings::{
        DisplayMarker, DisplayProxy, DisplaySettings, IntlMarker, LowLightMode as FidlLowLightMode,
        Theme as FidlTheme, ThemeMode as FidlThemeMode, ThemeType as FidlThemeType,
    },
    fuchsia_async as fasync,
    fuchsia_zircon::{self as zx, Status},
    futures::future::BoxFuture,
    futures::lock::Mutex,
    futures::prelude::*,
    matches::assert_matches,
    std::sync::Arc,
};

const ENV_NAME: &str = "settings_service_display_test_environment";
const STARTING_BRIGHTNESS: f32 = 0.5;
const CHANGED_BRIGHTNESS: f32 = 0.8;
const CONTEXT_ID: u64 = 0;

async fn setup_display_env() -> DisplayProxy {
    let env = EnvironmentBuilder::new(InMemoryStorageFactory::create())
        .settings(&[SettingType::Display])
        .spawn_and_get_nested_environment(ENV_NAME)
        .await
        .unwrap();

    env.connect_to_service::<DisplayMarker>().unwrap()
}

async fn setup_brightness_display_env() -> (DisplayProxy, BrightnessService) {
    let service_registry = ServiceRegistry::create();
    let brightness_service_handle = BrightnessService::create();
    service_registry
        .lock()
        .await
        .register_service(Arc::new(Mutex::new(brightness_service_handle.clone())));

    let env = EnvironmentBuilder::new(InMemoryStorageFactory::create())
        .service(Box::new(ServiceRegistry::serve(service_registry)))
        .settings(&[SettingType::Display])
        .flags(&[ControllerFlag::ExternalBrightnessControl])
        .spawn_and_get_nested_environment(ENV_NAME)
        .await
        .unwrap();

    (env.connect_to_service::<DisplayMarker>().unwrap(), brightness_service_handle)
}

// Creates an environment that will fail on a get request.
async fn create_display_test_env_with_failures(
    storage_factory: Arc<Mutex<InMemoryStorageFactory>>,
) -> DisplayProxy {
    create_test_env_with_failures(storage_factory, ENV_NAME, SettingType::Display)
        .await
        .connect_to_service::<DisplayMarker>()
        .unwrap()
}

// Tests that the FIDL calls for manual brightness result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_manual_brightness_with_storage_controller() {
    let display_proxy = setup_display_env().await;

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.brightness_value, Some(STARTING_BRIGHTNESS));

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.brightness_value = Some(CHANGED_BRIGHTNESS);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.brightness_value, Some(CHANGED_BRIGHTNESS));
}

// Tests that the FIDL calls for manual brightness result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_manual_brightness_with_brightness_controller() {
    let (display_proxy, brightness_service_handle) = setup_brightness_display_env().await;

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.brightness_value, Some(STARTING_BRIGHTNESS));

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.brightness_value = Some(CHANGED_BRIGHTNESS);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.brightness_value, Some(CHANGED_BRIGHTNESS));

    let current_brightness =
        brightness_service_handle.get_manual_brightness().lock().await.expect("get successful");
    assert_eq!(current_brightness, CHANGED_BRIGHTNESS);
}

// Tests that the FIDL calls for auto brightness result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_auto_brightness_with_storage_controller() {
    let display_proxy = setup_display_env().await;

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.auto_brightness = Some(true);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.auto_brightness, Some(true));
}

// Tests that the FIDL calls for auto brightness result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_auto_brightness_with_brightness_controller() {
    let (display_proxy, brightness_service_handle) = setup_brightness_display_env().await;

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.auto_brightness = Some(true);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.auto_brightness, Some(true));

    let auto_brightness =
        brightness_service_handle.get_auto_brightness().lock().await.expect("get successful");
    assert!(auto_brightness);
}

// Tests that the FIDL calls for light mode result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_light_mode_with_storage_controller() {
    let display_proxy = setup_display_env().await;

    // Test that if display is enabled, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.low_light_mode = Some(FidlLowLightMode::Enable);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.low_light_mode, Some(FidlLowLightMode::Enable));

    // Test that if display is disabled, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.low_light_mode = Some(FidlLowLightMode::Disable);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.low_light_mode, Some(FidlLowLightMode::Disable));

    // Test that if display is disabled immediately, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.low_light_mode = Some(FidlLowLightMode::DisableImmediately);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.low_light_mode, Some(FidlLowLightMode::DisableImmediately));
}

// Tests that the FIDL calls for light mode result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_light_mode_with_brightness_controller() {
    let (display_proxy, _) = setup_brightness_display_env().await;

    // Test that if display is enabled, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.low_light_mode = Some(FidlLowLightMode::Enable);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.low_light_mode, Some(FidlLowLightMode::Enable));

    // Test that if display is disabled, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.low_light_mode = Some(FidlLowLightMode::Disable);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.low_light_mode, Some(FidlLowLightMode::Disable));

    // Test that if display is disabled immediately, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.low_light_mode = Some(FidlLowLightMode::DisableImmediately);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.low_light_mode, Some(FidlLowLightMode::DisableImmediately));
}

// Tests for display theme.
#[fuchsia_async::run_until_stalled(test)]
async fn test_theme_type_light() {
    let incoming_theme =
        Some(FidlTheme { theme_type: Some(FidlThemeType::Light), ..FidlTheme::EMPTY });
    let expected_theme = incoming_theme.clone();

    let display_proxy = setup_display_env().await;

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.theme = incoming_theme;
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.theme, expected_theme);
}

#[fuchsia_async::run_until_stalled(test)]
async fn test_no_theme_set() {
    let display_proxy = setup_display_env().await;

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.theme, Some(FidlTheme::EMPTY));
}

#[fuchsia_async::run_until_stalled(test)]
async fn test_theme_mode_auto() {
    let incoming_theme =
        Some(FidlTheme { theme_mode: Some(FidlThemeMode::Auto), ..FidlTheme::EMPTY });
    // TODO(fxb/64775): Once we remove ThemeType.AUTO, the incoming and expected
    // values should be the same.
    let expected_theme = Some(FidlTheme {
        theme_type: Some(FidlThemeType::Auto),
        ..incoming_theme.clone().unwrap()
    });

    let display_proxy = setup_display_env().await;

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.theme = incoming_theme;
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.theme, expected_theme);
}

// TODO(fxb/64775) Remove this test once we remove the theme type of AUTO.
#[fuchsia_async::run_until_stalled(test)]
async fn test_theme_type_auto() {
    let incoming_theme =
        Some(FidlTheme { theme_type: Some(FidlThemeType::Auto), ..FidlTheme::EMPTY });
    let expected_theme = Some(FidlTheme {
        theme_mode: Some(FidlThemeMode::Auto),
        ..incoming_theme.clone().unwrap()
    });

    let display_proxy = setup_display_env().await;

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.theme = incoming_theme;
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.theme, expected_theme);
}

#[fuchsia_async::run_until_stalled(test)]
async fn test_theme_mode_auto_and_type_light() {
    let incoming_theme = Some(FidlTheme {
        theme_mode: Some(FidlThemeMode::Auto),
        theme_type: Some(FidlThemeType::Light),
        ..FidlTheme::EMPTY
    });
    let expected_theme = incoming_theme.clone();

    let display_proxy = setup_display_env().await;

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.theme = incoming_theme;
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.theme, expected_theme);
}

#[fuchsia_async::run_until_stalled(test)]
async fn test_theme_mode_auto_preserves_previous_type() {
    let first_incoming_theme =
        Some(FidlTheme { theme_type: Some(FidlThemeType::Light), ..FidlTheme::EMPTY });
    let second_incoming_theme =
        Some(FidlTheme { theme_mode: Some(FidlThemeMode::Auto), ..FidlTheme::EMPTY });
    let expected_theme = Some(FidlTheme {
        theme_mode: Some(FidlThemeMode::Auto),
        theme_type: Some(FidlThemeType::Light),
        ..FidlTheme::EMPTY
    });

    let display_proxy = setup_display_env().await;

    let mut first_display_settings = DisplaySettings::EMPTY;
    first_display_settings.theme = first_incoming_theme;
    display_proxy
        .set(first_display_settings)
        .await
        .expect("set completed")
        .expect("set successful");

    let mut second_display_settings = DisplaySettings::EMPTY;
    second_display_settings.theme = second_incoming_theme;
    display_proxy
        .set(second_display_settings)
        .await
        .expect("set completed")
        .expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");
    assert_eq!(settings.theme, expected_theme);
}

// Tests that the FIDL calls for screen enabled result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_screen_enabled_with_storage_controller() {
    let display_proxy = setup_display_env().await;
    test_screen_enabled(display_proxy).await;
}

// Tests that the FIDL calls for screen enabled result in appropriate
// commands sent to the switchboard.
#[fuchsia_async::run_until_stalled(test)]
async fn test_screen_enabled_with_brightness_controller() {
    let (display_proxy, _) = setup_brightness_display_env().await;
    test_screen_enabled(display_proxy).await;
}

async fn test_screen_enabled(display_proxy: DisplayProxy) {
    // Test that if screen is turned off, it is reflected.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.auto_brightness = Some(false);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.screen_enabled = Some(false);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.screen_enabled, Some(false));

    // Test that if display is turned back on, the display and manual brightness are on.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.screen_enabled = Some(true);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.screen_enabled, Some(true));
    assert_eq!(settings.auto_brightness, Some(false));

    // Test that if auto brightness is turned on, the display and auto brightness are on.
    let mut display_settings = DisplaySettings::EMPTY;
    display_settings.auto_brightness = Some(true);
    display_proxy.set(display_settings).await.expect("set completed").expect("set successful");

    let settings = display_proxy.watch().await.expect("watch completed");

    assert_eq!(settings.auto_brightness, Some(true));
    assert_eq!(settings.screen_enabled, Some(true));
}

// Makes sure that settings are restored from storage when service comes online.
#[fuchsia_async::run_until_stalled(test)]
async fn test_display_restore_with_storage_controller() {
    // Ensure auto-brightness value is restored correctly.
    validate_restore_with_storage_controller(0.7, true, true, LowLightMode::Enable, None).await;

    // Ensure manual-brightness value is restored correctly.
    validate_restore_with_storage_controller(0.9, false, true, LowLightMode::Disable, None).await;
}

async fn validate_restore_with_storage_controller(
    manual_brightness: f32,
    auto_brightness: bool,
    screen_enabled: bool,
    low_light_mode: LowLightMode,
    theme: Option<Theme>,
) {
    let service_registry = ServiceRegistry::create();
    let storage_factory = InMemoryStorageFactory::create();
    {
        let store = storage_factory
            .lock()
            .await
            .get_device_storage::<DisplayInfo>(StorageAccessContext::Test, CONTEXT_ID);
        let info = DisplayInfo {
            manual_brightness_value: manual_brightness,
            auto_brightness,
            screen_enabled,
            low_light_mode,
            theme,
        };
        assert!(store.lock().await.write(&info, false).await.is_ok());
    }

    let env = EnvironmentBuilder::new(storage_factory)
        .service(Box::new(ServiceRegistry::serve(service_registry)))
        .agents(&[restore_agent::blueprint::create()])
        .settings(&[SettingType::Display])
        .spawn_and_get_nested_environment(ENV_NAME)
        .await
        .ok();

    assert!(env.is_some());

    let display_proxy = env.unwrap().connect_to_service::<DisplayMarker>().unwrap();
    let settings = display_proxy.watch().await.expect("watch completed");

    if auto_brightness {
        assert_eq!(settings.auto_brightness, Some(auto_brightness));
    } else {
        assert_eq!(settings.brightness_value, Some(manual_brightness));
    }
}

// Makes sure that settings are restored from storage when service comes online.
#[fuchsia_async::run_until_stalled(test)]
async fn test_display_restore_with_brightness_controller() {
    // Ensure auto-brightness value is restored correctly.
    validate_restore_with_brightness_controller(0.7, true, true, LowLightMode::Enable, None).await;

    // Ensure manual-brightness value is restored correctly.
    validate_restore_with_brightness_controller(0.9, false, true, LowLightMode::Disable, None)
        .await;
}

async fn validate_restore_with_brightness_controller(
    manual_brightness: f32,
    auto_brightness: bool,
    screen_enabled: bool,
    low_light_mode: LowLightMode,
    theme: Option<Theme>,
) {
    let service_registry = ServiceRegistry::create();
    let brightness_service_handle = BrightnessService::create();
    service_registry
        .lock()
        .await
        .register_service(Arc::new(Mutex::new(brightness_service_handle.clone())));
    let storage_factory = InMemoryStorageFactory::create();
    {
        let store = storage_factory
            .lock()
            .await
            .get_device_storage::<DisplayInfo>(StorageAccessContext::Test, CONTEXT_ID);
        let info = DisplayInfo {
            manual_brightness_value: manual_brightness,
            auto_brightness,
            screen_enabled,
            low_light_mode,
            theme,
        };
        assert!(store.lock().await.write(&info, false).await.is_ok());
    }

    assert!(EnvironmentBuilder::new(storage_factory)
        .service(Box::new(ServiceRegistry::serve(service_registry)))
        .agents(&[restore_agent::blueprint::create()])
        .settings(&[SettingType::Display])
        .flags(&[ControllerFlag::ExternalBrightnessControl])
        .spawn_and_get_nested_environment(ENV_NAME)
        .await
        .is_ok());

    if auto_brightness {
        let service_auto_brightness =
            brightness_service_handle.get_auto_brightness().lock().await.unwrap();
        assert_eq!(service_auto_brightness, auto_brightness);
    } else {
        let service_manual_brightness =
            brightness_service_handle.get_manual_brightness().lock().await.unwrap();
        assert_eq!(service_manual_brightness, manual_brightness);
    }
}

// Makes sure that a failing display stream doesn't cause a failure for a different interface.
#[fuchsia_async::run_until_stalled(test)]
async fn test_display_failure() {
    let service_gen = |service_name: &str,
                       channel: zx::Channel|
     -> BoxFuture<'static, Result<(), anyhow::Error>> {
        match service_name {
            fidl_fuchsia_ui_brightness::ControlMarker::NAME => {
                // This stream is closed immediately
                let manager_stream_result =
                    ServerEnd::<fidl_fuchsia_ui_brightness::ControlMarker>::new(channel)
                        .into_stream();

                if manager_stream_result.is_err() {
                    return Box::pin(async {
                        Err(format_err!("could not move brightness channel into stream"))
                    });
                }
                return Box::pin(async { Ok(()) });
            }
            fidl_fuchsia_deprecatedtimezone::TimezoneMarker::NAME => {
                let timezone_stream_result =
                    ServerEnd::<fidl_fuchsia_deprecatedtimezone::TimezoneMarker>::new(channel)
                        .into_stream();

                if timezone_stream_result.is_err() {
                    return Box::pin(async {
                        Err(format_err!("could not move timezone channel into stream"))
                    });
                }
                let mut timezone_stream = timezone_stream_result.unwrap();
                fasync::Task::spawn(async move {
                    while let Some(req) = timezone_stream.try_next().await.unwrap() {
                        match req {
                            fidl_fuchsia_deprecatedtimezone::TimezoneRequest::GetTimezoneId {
                                responder,
                            } => {
                                responder.send("PDT").unwrap();
                            }
                            _ => {}
                        }
                    }
                })
                .detach();
                return Box::pin(async { Ok(()) });
            }
            _ => Box::pin(async { Err(format_err!("unsupported")) }),
        }
    };

    let env = EnvironmentBuilder::new(InMemoryStorageFactory::create())
        .service(Box::new(service_gen))
        .settings(&[SettingType::Display, SettingType::Intl])
        .spawn_and_get_nested_environment(ENV_NAME)
        .await
        .unwrap();

    let display_proxy = env.connect_to_service::<DisplayMarker>().expect("connected to service");

    let _settings_value = display_proxy.watch().await.expect("watch completed");

    let intl_service = env.connect_to_service::<IntlMarker>().unwrap();
    let _settings = intl_service.watch().await.expect("watch completed");
}

#[fuchsia_async::run_until_stalled(test)]
async fn test_channel_failure_watch() {
    let display_proxy =
        create_display_test_env_with_failures(InMemoryStorageFactory::create()).await;
    let result = display_proxy.watch().await;
    assert_matches!(result, Err(ClientChannelClosed { status: Status::UNAVAILABLE, .. }));
}
