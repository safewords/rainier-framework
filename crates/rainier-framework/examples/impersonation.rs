//! Support staff acting as a customer, offline.
//!
//! What it shows:
//!
//! 1. a [`SessionGuard`] logging an admin in, behind `StartSession`, with the
//!    session cookie carried from request to request;
//! 2. [`Impersonation`] with a policy — admins only, and never another admin;
//! 3. after `take`, the same guard resolving the **customer** on `/whoami`;
//! 4. [`ForbidImpersonation`] refusing `/billing` while impersonating;
//! 5. `leave` restoring the admin, and `/billing` working again;
//! 6. the policy refusing a non-admin, and refusing an admin target.
//!
//! ```sh
//! cargo run -p rainier-framework --example impersonation
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rainier_framework::auth::{
    impersonator_id, Authenticatable, Credentials, ForbidImpersonation, Guard, Impersonation,
    SessionGuard, StatefulGuard, UserProvider,
};
use rainier_framework::container::Container;
use rainier_framework::http::{Method, Request, Response};
use rainier_framework::routing::{GroupAttributes, Req, Router};
use rainier_framework::session::{MemorySessionStore, SessionStore, StartSession};
use rainier_framework::support::Result;

#[derive(Debug, Clone)]
struct User {
    id: u64,
    name: &'static str,
    admin: bool,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.to_string()
    }
    fn auth_password_hash(&self) -> Option<&str> {
        None
    }
}

/// Three users in memory. Credentials are only ever checked by id here — the
/// example logs people in directly with `login`.
struct Users(HashMap<String, User>);

#[async_trait::async_trait]
impl UserProvider<User> for Users {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.0.get(id).cloned())
    }
    async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }
    async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
        Ok(false)
    }
}

/// A browser: remembers the session cookie between requests.
struct Browser {
    router: rainier_framework::routing::CompiledRouter,
    cookie: Mutex<Option<String>>,
}

impl Browser {
    async fn request(&self, method: Method, path: &str) -> (u16, String) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(cookie) = self.cookie.lock().unwrap().clone() {
            builder = builder.header("cookie", &cookie);
        }
        let response = self.router.dispatch(builder.build()).await;

        if let Some(set) = response.header("set-cookie") {
            let pair = set.split(';').next().unwrap_or_default().to_string();
            *self.cookie.lock().unwrap() = Some(pair);
        }
        let status = response.status().as_u16();
        let bytes = response.into_http().into_body().collect().await.unwrap_or_default();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let users: HashMap<String, User> = [
        User { id: 1, name: "Ada (admin)", admin: true },
        User { id: 2, name: "Grace (admin)", admin: true },
        User { id: 7, name: "Linus (customer)", admin: false },
    ]
    .into_iter()
    .map(|user| (user.auth_identifier(), user))
    .collect();
    let users = Arc::new(Users(users));

    let guard =
        Arc::new(SessionGuard::new("web", Arc::clone(&users) as Arc<dyn UserProvider<User>>));
    let impersonation = Arc::new(
        // Admins only, and never another admin.
        Impersonation::new(|staff: &User, target: &User| staff.admin && !target.admin)
            .on_taken(|staff, target| println!("   [audit] {staff} began impersonating {target}"))
            .on_left(|staff, target| println!("   [audit] {staff} stopped impersonating {target}")),
    );

    let mut router = Router::new();
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::default());
    {
        let (guard, users, impersonation) =
            (Arc::clone(&guard), Arc::clone(&users), Arc::clone(&impersonation));
        router.group(GroupAttributes::new().middleware(StartSession::new(store)), move |router| {
            // Stand-in for a real login form.
            let login = {
                let (guard, users) = (Arc::clone(&guard), Arc::clone(&users));
                move |request: Req| {
                    let (guard, users) = (Arc::clone(&guard), Arc::clone(&users));
                    async move {
                        let id = request.route_param("user").unwrap_or_default().to_string();
                        let user = users.retrieve_by_id(&id).await?.expect("a known user");
                        guard.login(&request, &user).await?;
                        Ok::<_, rainier_framework::support::Error>(Response::text(format!(
                            "logged in as {}",
                            user.name
                        )))
                    }
                }
            };
            router.post("/login/{user}", login);

            let whoami = {
                let guard = Arc::clone(&guard);
                move |request: Req| {
                    let guard = Arc::clone(&guard);
                    async move {
                        let user = guard.user(&request).await?;
                        let real = impersonator_id(&request)
                            .map(|id| format!(" (really user {id})"))
                            .unwrap_or_default();
                        Ok::<_, rainier_framework::support::Error>(Response::text(format!(
                            "{}{real}",
                            user.map(|u| u.name).unwrap_or("guest")
                        )))
                    }
                }
            };
            router.get("/whoami", whoami);

            let take = {
                let (guard, users, impersonation) =
                    (Arc::clone(&guard), Arc::clone(&users), Arc::clone(&impersonation));
                move |request: Req| {
                    let (guard, users, impersonation) =
                        (Arc::clone(&guard), Arc::clone(&users), Arc::clone(&impersonation));
                    async move {
                        let staff = guard.user(&request).await?.expect("logged in");
                        let id = request.route_param("user").unwrap_or_default().to_string();
                        let target = users.retrieve_by_id(&id).await?.expect("a known user");
                        impersonation.take(&request, &staff, &target)?;
                        Ok::<_, rainier_framework::support::Error>(Response::text(format!(
                            "now viewing as {}",
                            target.name
                        )))
                    }
                }
            };
            router.post("/support/impersonate/{user}", take);

            let leave = {
                let impersonation = Arc::clone(&impersonation);
                move |request: Req| {
                    let impersonation = Arc::clone(&impersonation);
                    async move {
                        let back_to = impersonation.leave(&request)?;
                        Ok::<_, rainier_framework::support::Error>(Response::text(format!(
                            "back to user {back_to}"
                        )))
                    }
                }
            };
            router.post("/support/leave", leave);

            // Acts with the customer's authority, so never while impersonating.
            router
                .get("/billing", |_: Req| async { Response::text("payment methods") })
                .middleware(ForbidImpersonation);
        });
    }

    let browser = Browser { router: router.compile(&Container::new())?, cookie: Mutex::new(None) };
    let step = |n: &str, (status, body): (u16, String)| println!("{n} {status} {body}");

    step("1. POST /login/1               →", browser.request(Method::POST, "/login/1").await);
    step("   GET  /whoami                →", browser.request(Method::GET, "/whoami").await);
    step(
        "2. POST /support/impersonate/7 →",
        browser.request(Method::POST, "/support/impersonate/7").await,
    );
    step("3. GET  /whoami                →", browser.request(Method::GET, "/whoami").await);
    step("4. GET  /billing               →", browser.request(Method::GET, "/billing").await);
    step("5. POST /support/leave         →", browser.request(Method::POST, "/support/leave").await);
    step("   GET  /whoami                →", browser.request(Method::GET, "/whoami").await);
    step("   GET  /billing               →", browser.request(Method::GET, "/billing").await);

    println!("6. the policy says no:");
    step(
        "   admin → admin               →",
        browser.request(Method::POST, "/support/impersonate/2").await,
    );
    step("   POST /login/7 (customer)    →", browser.request(Method::POST, "/login/7").await);
    step(
        "   customer → admin            →",
        browser.request(Method::POST, "/support/impersonate/1").await,
    );

    Ok(())
}
