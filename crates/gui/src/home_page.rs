use crate::args::DownloadArgs;
use crate::manager::{Manager, ManagerData, Message};
use crate::persist::{Database, DatabaseEntry};
use crate::{fmt, progress};
use color_eyre::Result;
use fast_down::Total;
use slint::{Model, Timer, VecModel};
use std::sync::RwLock;
use std::{rc::Rc, sync::Arc};

slint::include_modules!();

impl From<&DatabaseEntry> for DownloadData {
    fn from(value: &DatabaseEntry) -> Self {
        let downloaded = value.progress.total();
        let speed = downloaded as f64 / value.elapsed as f64 * 1e3;
        let remaining_time = if speed > 0.0 {
            let remaining = (value.total_size - downloaded) as f64 / speed;
            fmt::format_time(remaining as u64)
        } else {
            "NaN".into()
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

pub fn home_page(args: DownloadArgs) -> Result<()> {
    let args = Arc::new(args);
    dbg!(&args);
    let ui = AppWindow::new()?;

    let db = Database::new()?;
    let entries = db.get_all_entries()?;
    let manager = Arc::new(Manager::new(
        args.clone(),
        entries
            .iter()
            .map(|e| {
                Arc::new(RwLock::new(ManagerData {
                    result: None,
                    url: e.url.clone(),
                    file_path: Some(e.file_path.clone()),
                }))
            })
            .collect::<Vec<_>>(),
    ));
    let download_list_model = Rc::new(VecModel::from_iter(entries.iter().map(|e| e.into())));
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
                    remaining_time: "NaN".into(),
                    speed: "0.00 B/s".into(),
                },
            );
            manager.add_task(url.to_string()).unwrap();
        }
    });

    ui.on_stop({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        move |index| {
            let index = index as usize;
            manager.stop(index).unwrap();
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
            manager.resume(index).unwrap();
        }
    });

    ui.on_delete({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        move |index| {
            let index = index as usize;
            manager.remove_task(index).unwrap();
            download_list_model.remove(index);
        }
    });

    let timer = Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        {
            let ui = ui.as_weak();
            move || {
                let manager = manager.clone();
                ui.upgrade_in_event_loop(move |ui| {
                    let download_list_model = ui.get_download_list();
                    while let Ok(msg) = manager.rx_recv.try_recv() {
                        match msg {
                            Message::ProgressUpdate(index, res) => {
                                let mut data = download_list_model.row_data(index).unwrap();
                                data.progress = Rc::new(VecModel::from(res)).into();
                                download_list_model.set_row_data(index, data);
                            }
                            Message::Stopped(index) => {
                                let mut data = download_list_model.row_data(index).unwrap();
                                data.is_downloading = false;
                                download_list_model.set_row_data(index, data);
                            }
                            Message::Started(index) => {
                                let mut data = download_list_model.row_data(index).unwrap();
                                data.is_downloading = true;
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
        },
    );

    ui.run()?;
    Ok(())
}
