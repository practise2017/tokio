use std::time::Duration;

use pyo3::*;
use futures::future::{self, Future};
use futures::sync::oneshot;
use tokio_core::reactor::Timeout;

use ::{TokioEventLoop, TokioEventLoopPtr, Classes};

#[py::class]
pub struct PyHandle {
    evloop: TokioEventLoopPtr,
    cancelled: bool,
    cancel_handle: Option<oneshot::Sender<()>>,
    callback: PyObject,
    args: PyTuple,
    source_traceback: Option<PyObject>,
    token: PyToken,
}

#[py::ptr(PyHandle)]
pub struct PyHandlePtr(PyPtr);

#[py::methods]
impl PyHandle {

    fn cancel(&mut self, _py: Python) -> PyResult<()> {
        self.cancelled = true;

        if let Some(tx) = self.cancel_handle.take() {
            let _ = tx.send(());
        }

        Ok(())
    }

    #[getter(_cancelled)]
    fn get_cancelled(&self, _py: Python) -> PyResult<bool> {
        Ok(self.cancelled)
    }
}


impl PyHandle {

    pub fn new(py: Python, evloop: &TokioEventLoop,
               callback: PyObject, args: PyTuple) -> PyResult<PyHandlePtr> {

        let tb = if evloop.is_debug() {
            let frame = Classes.Sys.call(py, "_getframe", (0,), None)?;
            Some(Classes.ExtractStack.call(py, (frame,), None)?)
        } else {
            None
        };

        py.init(|t| PyHandle{
            evloop: evloop.to_inst_ptr(),
            cancelled: false,
            cancel_handle: None,
            callback: callback,
            args: args,
            source_traceback: tb,
            token: t})
    }
}

impl PyHandlePtr {

    pub fn call_soon(&self, py: Python, evloop: &TokioEventLoop) {
        let h = self.clone_ref(py);

        // schedule work
        evloop.get_handle().spawn_fn(move || {
            h.run();
            future::ok(())
        });
    }

    pub fn call_soon_threadsafe(&self, py: Python, evloop: &TokioEventLoop) {
        let h = self.clone_ref(py);

        // schedule work
        evloop.remote().spawn(move |_| {
            h.run();
            future::ok(())
        });
    }

    pub fn call_later(&mut self, py: Python, evloop: &TokioEventLoop, when: Duration) {
        // cancel onshot
        let (cancel, rx) = oneshot::channel::<()>();
        self.as_mut(py).cancel_handle = Some(cancel);

        // we need to hold reference, otherwise python will release handle object
        let h = self.clone_ref(py);

        // start timer
        let fut = Timeout::new(when, evloop.href()).unwrap().select2(rx)
            .then(move |res| {
                if let Ok(future::Either::A(_)) = res {
                    // timeout got fired, call callback
                    h.run();
                }
                future::ok(())
            });
        evloop.href().spawn(fut);
    }

    pub fn run(&self) {
        let _: PyResult<()> = self.with(|py, h| {
            // check if cancelled
            if h.cancelled {
                return Ok(())
            }

            let result = h.callback.call(py, h.args.clone_ref(py), None);

            // handle python exception
            if let Err(err) = result {
                if err.matches(py, &Classes.Exception) {
                    let context = PyDict::new(py);
                    context.set_item(py, "message",
                                     format!("Exception in callback {:?} {:?}",
                                             h.callback, h.args))?;
                    context.set_item(py, "handle", format!("{:?}", h))?;
                    context.set_item(py, "exception", err.clone_ref(py).instance(py))?;

                    if let Some(ref tb) = h.source_traceback {
                        context.set_item(py, "source_traceback", tb.clone_ref(py))?;
                    }
                    h.evloop.as_ref(py).call_exception_handler(py, context)?;
                } else {
                    // escalate to event loop
                    h.evloop.as_mut(py).stop_with_err(py, err);
                }
            }
            Ok(())
        });
    }
}
