use crate::{
    args::DownloadArgs,
    fmt,
    manager::{Manager, ManagerData, Message},
    persist::{Database, DatabaseEntry},
    progress,
};
use color_eyre::Result;
use fast_down::Total;
use slint::{Model, Timer, TimerMode, VecModel};
use std::{
    rc::Rc,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
use tokio::sync::Mutex;

slint::include_modules!();

impl From<&DatabaseEntry> for DownloadData {
    fn from(value: &DatabaseEntry) -> Self {
        let downloaded = value.progress.total();
        let speed = downloaded as f64 / value.elapsed as f64 * 1e3;
        let remaining_time = if speed > 0.0 && value.total_size > 0 {
            let remaining = (value.total_size - downloaded) as f64 / speed;
            fmt::format_time(remaining as u64)
        } else {
            "Unknown".into()
        };
        Self {
            elapsed: fmt::format_time(value.elapsed / 1000).into(),
            file_name: value.file_name.clone().into(),
            is_downloading: false,
            percentage: format!(
                "{:.2}%",
                downloaded as f64 / value.total_size as f64 * 100.0
            )
            .into(),
            progress: Rc::new(VecModel::from(progress::add_blank(
                &value.progress,
                value.total_size,
            )))
            .into(),
            remaining_time: remaining_time.into(),
            speed: format!("{}/s", fmt::format_size(speed)).into(),
        }
    }
}

pub async fn home_page(args: DownloadArgs) -> Result<()> {
    let args = Arc::new(args);
    dbg!(&args);
    let ui = AppWindow::new()?;

    let db = Database::new().await?;
    let entries = db.get_all_entries().await?;
    let manager = Arc::new(Manager::new(
        args.clone(),
        entries
            .iter()
            .map(|e| {
                Arc::new(Mutex::new(ManagerData {
                    result: None,
                    url: e.url.clone(),
                    file_path: Some(e.file_path.clone()),
                    is_running: Arc::new(AtomicBool::new(false)),
                }))
            })
            .collect::<Vec<_>>(),
    ));
    let download_list_model = Rc::new(VecModel::from_iter(entries.iter().map(Into::into)));
    ui.set_download_list(download_list_model.clone().into());

    ui.on_add_url({
        let download_list_model = download_list_model.clone();
        let manager = manager.clone();
        move |url| {
            dbg!(&url);
            let progress_model = Rc::new(VecModel::from(vec![]));
            download_list_model.insert(
                0,
                DownloadData {
                    elapsed: "00:00:00".into(),
                    file_name: url.clone().into(),
                    is_downloading: true,
                    percentage: "0.00%".into(),
                    progress: progress_model.into(),
                    remaining_time: "Unknown".into(),
                    speed: "0.00 B/s".into(),
                },
            );
            let manager = manager.clone();
            slint::spawn_local(async move {
                manager.add_task(url.to_string()).await.unwrap();
            })
            .unwrap();
        }
    });

    ui.on_stop({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        move |index| {
            let index = index as usize;
            let manager = manager.clone();
            slint::spawn_local(async move {
                manager.stop(index).await.unwrap();
            })
            .unwrap();
            let mut data = download_list_model.row_data(index).unwrap();
            data.is_downloading = false;
            download_list_model.set_row_data(index, data);
        }
    });

    ui.on_resume({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        move |index| {
            let index = index as usize;
            let mut data = download_list_model.row_data(index).unwrap();
            data.is_downloading = true;
            download_list_model.set_row_data(index, data);
            let manager = manager.clone();
            slint::spawn_local(async move {
                manager.resume(index).await.unwrap();
            })
            .unwrap();
        }
    });

    ui.on_delete({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        move |index| {
            let index = index as usize;
            let manager = manager.clone();
            slint::spawn_local(async move {
                manager.remove_task(index).await.unwrap();
            })
            .unwrap();
            download_list_model.remove(index);
        }
    });

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), {
        let ui = ui.as_weak();
        move || {
            let manager = manager.clone();
            ui.upgrade_in_event_loop(move |ui| {
                let download_list_model = ui.get_download_list();
                while let Ok(msg) = manager.rx_recv.try_recv() {
                    match msg {
                        Message::Progress(index, res) => {
                            let mut data = download_list_model.row_data(index).unwrap();
                            data.progress = Rc::new(VecModel::from(res.progress)).into();
                            data.elapsed = res.elapsed.into();
                            data.percentage = res.percentage.into();
                            data.remaining_time = res.remaining_time.into();
                            data.speed = res.speed.into();
                            download_list_model.set_row_data(index, data);
                        }
                        Message::Stopped(index) => {
                            let mut data = download_list_model.row_data(index).unwrap();
                            data.is_downloading = false;
                            download_list_model.set_row_data(index, data);
                        }
                        Message::FileName(index, file_name) => {
                            let mut data = download_list_model.row_data(index).unwrap();
                            data.file_name = file_name.into();
                            download_list_model.set_row_data(index, data);
                        }
                    }
                }
            })
            .unwrap();
        }
    });

    ui.run()?;
    Ok(())
}
