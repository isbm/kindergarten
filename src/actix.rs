use std::{pin::Pin, sync::Arc};

use actix_web::{FromRequest, dev::Payload, web::Data};
use tokio::process::Command;

use crate::{Kind, Kindergarten, Ticket};

/// A struct that can provide a ticket
///
/// Usually this will usually be something that implements
/// [FromRequest] and can extract the ticket id from
/// a request
pub trait ChildProcess {
    fn ticket(&self) -> Ticket;
}

/// Extracts a ticket from the request and provides access to
/// a child process, by default all childrne are stored in the
/// same Kindergarten (which needs to be inserted on server start).
/// If you need multiple gardens for some reason, you can wrap them in
/// a new-type
pub struct Child<T: ChildProcess + FromRequest, Garten = Kindergarten>(Arc<Garten>, T);

impl<T: ChildProcess + FromRequest> Child<T> {
    pub async fn get(&self) -> Option<Kind> {
        self.0.get(self.1.ticket())
    }

    /// Returns a child or inserts it by running a command.
    ///
    /// Since spawning a new child is inherently fallible, this method is also fallible
    /// and there is no `get_or_try_insert_with`
    pub async fn get_or_spawn_with(&self, f: impl Fn() -> Command) -> std::io::Result<Kind> {
        self.0.get_or_insert_with(self.1.ticket(), f)
    }
}

impl<T: ChildProcess + FromRequest, G: 'static> FromRequest for Child<T, G>
where
    T: 'static,
    <T as FromRequest>::Error: std::error::Error + 'static,
{
    type Error = Box<dyn std::error::Error + 'static>;

    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &actix_web::HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let ticket_provider = T::from_request(&req, &mut Payload::None).await?;
            let kinder_garten = Data::<G>::from_request(&req, &mut Payload::None).await?;
            Ok(Child(kinder_garten.into_inner(), ticket_provider))
        })
    }
}
