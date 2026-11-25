use super::*;
use crate::model::tests::stub_boot;
use gpui::TestAppContext;
use muxy_app_core::AppState;

#[gpui::test]
fn newer_release_replaces_downloaded_update_and_preserves_schedule(cx: &mut TestAppContext) {
    let (boot, requests) = stub_boot(AppState::bootstrap().expect("state"));
    let (view, cx) = cx.add_window_view(|window, cx| AppModel::new(boot, window, cx));
    view.update(cx, |model, cx| {
        model.servers.local.connection = ConnectionState::Ready;
        model.updates.server = Some(muxy_protocol::ServerInfo::current());
        model.updates.ready = Some(PreparedUpdate::fixture());
        model.updates.scheduled = true;
        model.updates.retry_preparation = Some(model.servers.local.generation);
        model.updates.mode = Some(UpdateMode::EndSessions);
        model.updates.download = AppUpdatePhase::Checking;
        let release = model
            .receive_update_release(Ok(Some(Release::fixture("2.0.0-beta.1236"))), cx)
            .expect("newer release");
        assert_eq!(release.version, "2.0.0-beta.1236");
        assert!(model.updates.download == AppUpdatePhase::Downloading);
        model.begin_update(cx);
        assert!(model.quitting == Quitting::Idle);
        model.receive_server_update(
            Ok(ServerUpdate {
                server: muxy_protocol::ServerInfo::current(),
                sessions: 0,
                replaced: false,
            }),
            cx,
        );
        assert!(model.quitting == Quitting::Idle);
        let mut latest = PreparedUpdate::fixture();
        latest.version = release.version;
        model.finish_update_check(Ok(Some(latest)), cx);
        assert!(!model.updates.checking());
        assert!(model.updates.scheduled);
        assert!(model.updates.retry_preparation.is_none());
        assert!(model.updates.mode.is_none());
        assert_eq!(
            model.updates.ready.as_ref().expect("update").version,
            "2.0.0-beta.1236"
        );
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(model.path.with_file_name("pending-update.json")).expect("record"),
        )
        .expect("json");
        assert_eq!(record["version"], "2.0.0-beta.1236");
        assert_eq!(record["scheduled"], true);
    });
    assert!(
        !requests
            .try_iter()
            .any(|(_, work)| matches!(work, Work::Flush | Work::PrepareUpdate { .. }))
    );
}

#[gpui::test]
fn disabling_automatic_app_update_checks_skips_background_but_keeps_manual(
    cx: &mut TestAppContext,
) {
    let (mut boot, _requests) = stub_boot(AppState::bootstrap().expect("state"));
    boot.settings.privacy.check_app_updates = false;
    let (view, cx) = cx.add_window_view(|window, cx| AppModel::new(boot, window, cx));
    view.update(cx, |model, cx| {
        model.updates.installation = Some(Installation::fixture());
        model.updates.task = None;
        model.check_for_updates(false, cx);
        assert!(!model.updates.checking());
        assert!(model.updates.task.is_none());
        model.updates.installation = None;
        model.check_for_updates(true, cx);
    });
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
}
