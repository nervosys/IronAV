//! Let the platform file dialog complete without waiting in egui's event loop.

use eframe::egui;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

struct Repaint(egui::Context);

impl Wake for Repaint {
    fn wake(self: Arc<Self>) {
        self.0.request_repaint();
    }
}

struct Dialog<T> {
    purpose: T,
    result: Pin<Box<dyn Future<Output = Option<rfd::FileHandle>>>>,
    waker: Waker,
}

pub(crate) struct Picker<T>(Option<Dialog<T>>);

impl<T> Default for Picker<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> Picker<T> {
    pub fn busy(&self) -> bool {
        self.0.is_some()
    }

    pub fn open(&mut self, purpose: T, dialog: rfd::AsyncFileDialog, ctx: &egui::Context) {
        if self.0.is_none() {
            self.0 = Some(Dialog { purpose, result: Box::pin(dialog.pick_file()), waker: Waker::from(Arc::new(Repaint(ctx.clone()))) });
            ctx.request_repaint();
        }
    }

    pub fn poll(&mut self) -> Option<(T, PathBuf)> {
        let dialog = self.0.as_mut()?;
        let Poll::Ready(file) = dialog.result.as_mut().poll(&mut Context::from_waker(&dialog.waker)) else { return None };
        let dialog = self.0.take()?;
        file.map(|f| (dialog.purpose, f.path().to_owned()))
    }
}
