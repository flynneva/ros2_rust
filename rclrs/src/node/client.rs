use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::error::{ClientErrorCode, ToResult};
use crate::future::RclFuture;
use crate::MessageCow;
use crate::{rcl_bindings::*, RclReturnCode};
use crate::{Node, NodeHandle};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::borrow::Borrow;
use cstr_core::CString;
use parking_lot::{Mutex, MutexGuard};
use rosidl_runtime_rs::Message;

pub struct ClientHandle {
    handle: Mutex<rcl_client_t>,
    node_handle: Arc<NodeHandle>,
}

impl ClientHandle {
    pub fn lock(&self) -> MutexGuard<rcl_client_t> {
        self.handle.lock()
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        let handle = self.handle.get_mut();
        let node_handle = &mut *self.node_handle.lock();
        unsafe {
            rcl_client_fini(handle as *mut _, node_handle as *mut _);
        }
    }
}

/// Trait to be implemented by concrete Client structs
/// See [`Client<T>`] for an example
pub trait ClientBase {
    fn handle(&self) -> &ClientHandle;
    fn execute(&self) -> Result<(), RclReturnCode>;
}

/// Main class responsible for publishing data to ROS topics
pub struct Client<T>
where
    T: rosidl_runtime_rs::Service,
{
    pub(crate) handle: Arc<ClientHandle>,
    requests: Mutex<HashMap<i64, Mutex<Box<dyn FnMut(&T::Response) + 'static>>>>,
    futures: Mutex<HashMap<i64, Arc<Mutex<Box<RclFuture<T::Response>>>>>>,
    sequence_number: AtomicI64,
}

impl<T> Client<T>
where
    T: rosidl_runtime_rs::Service,
{
    pub fn new(node: &Node, topic: &str) -> Result<Self, RclReturnCode>
    where
        T: rosidl_runtime_rs::Service,
    {
        let mut client_handle = unsafe { rcl_get_zero_initialized_client() };
        let type_support = <T as rosidl_runtime_rs::Service>::get_type_support()
            as *const rosidl_service_type_support_t;
        let topic_c_string = CString::new(topic).unwrap();
        let node_handle = &mut *node.handle.lock();

        unsafe {
            let client_options = rcl_client_get_default_options();

            rcl_client_init(
                &mut client_handle as *mut _,
                node_handle as *mut _,
                type_support,
                topic_c_string.as_ptr(),
                &client_options as *const _,
            )
            .ok()?;
        }

        let handle = Arc::new(ClientHandle {
            handle: Mutex::new(client_handle),
            node_handle: node.handle.clone(),
        });

        Ok(Self {
            handle,
            requests: Mutex::new(HashMap::new()),
            futures: Mutex::new(HashMap::new()),
            sequence_number: AtomicI64::new(0),
        })
    }

    /// Send a requests with a callback as a parameter.
    ///
    /// The [`MessageCow`] trait is implemented by any
    /// [`Message`] as well as any reference to a `Message`.
    ///
    /// The reason for allowing owned messages is that publishing owned messages can be more
    /// efficient in the case of idiomatic messages[^note].
    ///
    /// [^note]: See the [`Message`] trait for an explanation of "idiomatic".
    ///
    /// Hence, when a message will not be needed anymore after publishing, pass it by value.
    /// When a message will be needed again after publishing, pass it by reference, instead of cloning and passing by value.
    pub fn async_send_request_with_callback<'a, M: MessageCow<'a, T::Request>, F>(
        &self,
        message: M,
        callback: F,
    ) -> Result<(), RclReturnCode>
    where
        F: FnMut(&T::Response) + Sized + 'static,
    {
        let rmw_message = T::Request::into_rmw_message(message.into_cow());
        let handle = &mut *self.handle.lock();
        let mut sequence_number = self.sequence_number.load(Ordering::SeqCst);
        let ret = unsafe {
            rcl_send_request(
                handle as *mut _,
                rmw_message.as_ref() as *const <T::Request as Message>::RmwMsg as *mut _,
                &mut sequence_number,
            )
        };
        let requests = &mut *self.requests.lock();
        requests.insert(sequence_number, Mutex::new(Box::new(callback)));
        self.sequence_number.swap(sequence_number, Ordering::SeqCst);
        ret.ok()
    }

    /// Send a requests with a callback as a parameter.
    ///
    /// The [`MessageCow`] trait is implemented by any
    /// [`Message`] as well as any reference to a `Message`.
    ///
    /// The reason for allowing owned messages is that publishing owned messages can be more
    /// efficient in the case of idiomatic messages[^note].
    ///
    /// [^note]: See the [`Message`] trait for an explanation of "idiomatic".
    ///
    /// Hence, when a message will not be needed anymore after publishing, pass it by value.
    /// When a message will be needed again after publishing, pass it by reference, instead of cloning and passing by value.
    pub fn call_async<'a, R: MessageCow<'a, T::Request>>(
        &self,
        request: R,
    ) -> Result<Arc<Mutex<Box<RclFuture<T::Response>>>>, RclReturnCode>
    where
        T: rosidl_runtime_rs::Service + 'static,
    {
        let rmw_message = T::Request::into_rmw_message(request.into_cow());
        let handle = &mut *self.handle.lock();
        let mut sequence_number = self.sequence_number.load(Ordering::SeqCst);
        let ret = unsafe {
            rcl_send_request(
                handle as *mut _,
                rmw_message.as_ref() as *const <T::Request as Message>::RmwMsg as *mut _,
                &mut sequence_number,
            )
        };
        let response = Arc::new(Mutex::new(Box::new(RclFuture::<T::Response>::new())));
        {
            let futures = &mut *self.futures.lock();
            futures.insert(sequence_number, response.clone());
        }
        self.sequence_number.swap(sequence_number, Ordering::SeqCst);
        ret.ok()?;
        Ok(response)
    }

    /// Ask RMW for the data
    ///
    /// +----------------------+
    /// | rclrs::take_response |
    /// +----------+-----------+
    ///            |
    ///            |
    /// +----------v-----------+
    /// |   rcl_take_response  |
    /// +----------+-----------+
    ///            |
    ///            |
    /// +----------v----------+
    /// |      rmw_take       |
    /// +---------------------+
    pub fn take_response(&self) -> Result<(T::Response, rmw_request_id_t), RclReturnCode> {
        let mut request_id_out = rmw_request_id_t {
            writer_guid: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            sequence_number: 0,
        };
        type RmwMsg<T> =
            <<T as rosidl_runtime_rs::Service>::Response as rosidl_runtime_rs::Message>::RmwMsg;
        let mut response_out = RmwMsg::<T>::default();
        let handle = &mut *self.handle.lock();
        let ret = unsafe {
            rcl_take_response(
                handle as *const _,
                &mut request_id_out,
                &mut response_out as *mut RmwMsg<T> as *mut _,
            )
        };
        ret.ok()?;
        Ok((T::Response::from_rmw_message(response_out), request_id_out))
    }
}

impl<T> ClientBase for Client<T>
where
    T: rosidl_runtime_rs::Service,
{
    fn handle(&self) -> &ClientHandle {
        self.handle.borrow()
    }

    fn execute(&self) -> Result<(), RclReturnCode> {
        let (res, req_id) = match self.take_response() {
            Ok((res, req_id)) => (res, req_id),
            Err(RclReturnCode::ClientError(ClientErrorCode::ClientTakeFailed)) => {
                // Spurious wakeup – this may happen even when a waitset indicated that this
                // subscription was ready, so it shouldn't be an error.
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let requests = &mut *self.requests.lock();
        let futures = &mut *self.futures.lock();
        if requests.contains_key(&req_id.sequence_number) {
            let callback = requests.remove(&req_id.sequence_number).unwrap();
            (&mut *callback.lock())(&res);
        } else if futures.contains_key(&req_id.sequence_number) {
            let future = futures.remove(&req_id.sequence_number).unwrap();
            (&mut *future.lock()).set_value(res);
        }
        Ok(())
    }
}
