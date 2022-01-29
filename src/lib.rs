use chrono::Utc;
use reqwest::header::COOKIE;
use serde::*;
use serde_json::{json, Value};
use std::fmt;
use worker::*;

mod utils;

fn log_request(req: &Request) {
    console_log!(
        "{} - [{}], located at: {:?}, within: {}",
        Date::now().to_string(),
        req.path(),
        req.cf().coordinates().unwrap_or_default(),
        req.cf().region().unwrap_or("unknown region".into())
    );
}

async fn check_user(ctx: &RouteContext<()>) -> Result<Vec<String>> {
    let kv = ctx.kv("users")?;
    let keys = kv.list().execute().await?.keys;
    let mut users = vec![];
    for key in keys {
        users.push(key.name);
    }
    Ok(users)
}

async fn add_user(username: &String, now: &String, ctx: &RouteContext<()>) {
    let kv = ctx.kv("users").unwrap();
    kv.put(&username, &now).unwrap().execute().await.unwrap();
}

#[event(fetch)]
pub async fn main(req: Request, env: Env) -> Result<Response> {
    log_request(&req);

    // Optionally, get more helpful error messages written to the console in the case of a panic.
    utils::set_panic_hook();

    // Optionally, use the Router to handle matching endpoints, use ":name" placeholders, or "*name"
    // catch-alls to match on specific patterns. Alternatively, use `Router::with_data(D)` to
    // provide arbitrary data that will be accessible in each route via the `ctx.data()` method.
    let router = Router::new();

    #[derive(Serialize, Deserialize, Debug)]
    struct Post {
        title: String,
        username: String,
        content: String,
    }

    impl fmt::Display for Post {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "{{ \"title\": {}, \"username\": {}, \"content\": {} }}",
                self.title, self.username, self.content
            )
        }
    }

    // Add as many routes as your Worker needs! Each route will get a `Request` for handling HTTP
    // functionality and a `RouteContext` which you can use to  and get route parameters and
    // Environment bindings like KV Stores, Durable Objects, Secrets, and Variables.
    router
        .get("/", |_, _| Response::ok("Hello from Workers!"))
        .post_async("/form/:field", |mut req, ctx| async move {
            if let Some(name) = ctx.param("field") {
                let form = req.form_data().await?;
                match form.get(name) {
                    Some(FormEntry::Field(value)) => {
                        return Response::from_json(&json!({ name: value }))
                    }
                    Some(FormEntry::File(_)) => {
                        return Response::error("`field` param in form shouldn't be a File", 422);
                    }
                    None => return Response::error("Bad Request", 400),
                }
            }

            Response::error("Bad Request", 400)
        })
        .get("/worker-version", |_, ctx| {
            let version = ctx.var("WORKERS_RS_VERSION")?.to_string();
            Response::ok(version)
        })
        .get_async("/posts", |_req, ctx| async move {
            let kv = ctx.kv("my-app-general_posts_preview")?;
            let keys = kv.list().execute().await?.keys;
            let mut posts: Vec<Value> = vec![];
            for key in keys {
                let value = kv.get(&key.name).await.unwrap().unwrap().as_string();
                let j = json!(value);
                posts.push(j);
            }
            console_log!("{:#?}", posts);
            let mut res = Response::from_json(&posts)?;
            let headers = Response::headers_mut(&mut res);
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                "https://cf-social-media-frontend.pages.dev",
            )?;
            Headers::set(headers, "Access-Control-Allow-Credentials", "true")?;
            Headers::set(headers, "transfer-encoding", "chunked")?;
            Headers::set(headers, "vary", "Accept-Encoding")?;
            Headers::set(headers, "connection", "keep-alive")?;
            Ok(res)
        })
        .post_async("/posts", |mut req, ctx| async move {
            let mut new_post: Value = req.json::<serde_json::Value>().await?;
            let now = Utc::now().to_rfc3339().to_string();
            let req_cookie = req.headers().get("Cookie")?.unwrap_or("".to_string());
            let name_not_found = "".to_string();
            *new_post.get_mut("time").unwrap() = serde_json::Value::String(now.clone());

            // * Get username and remove double quotes from name
            let mut username = match new_post.get("username") {
                Some(n) => n.to_string(),
                None => name_not_found.to_string(),
            };
            username.pop();
            username.remove(0);
            
            let mut res = Response::ok(format!("{}", new_post))?;
            let headers = Response::headers_mut(&mut res);
            let users = crate::check_user(&ctx).await?;

            if users.contains(&username) {
                if req_cookie.len() > 0 {
                    console_log!("This is the req cookie 🍪: {}", req_cookie);
                    let client = reqwest::Client::new();
                    let auth_resp = client
                        .get("https://tracked-specialized-webcam-mounts.trycloudflare.com/verify")
                        .header(COOKIE, req_cookie)
                        .send()
                        .await
                        .unwrap();
                    let resp_body = auth_resp.text().await.unwrap();
                    console_log!("resp_body {}", resp_body);
                    if resp_body != username {
                        return Response::error("Could not verify user", 401);
                    }
                }
            } else {
                // * Add new user to users KV
                crate::add_user(&username, &now, &ctx).await;

                // * Get the set-cookie header from authorization server
                let auth_resp = reqwest::get(format!(
                    "https://tracked-specialized-webcam-mounts.trycloudflare.com/auth/{}",
                    username
                ))
                .await
                .unwrap();
                let auth_resp_headers = auth_resp.headers();
                let set_cookie_header = auth_resp_headers
                    .get("set-cookie")
                    .unwrap()
                    .to_str()
                    .unwrap();
                Headers::set(headers, "Set-Cookie", set_cookie_header)?;
            }

            let new_post_string = new_post.to_string();

            // * Add post to kv
            let kv = ctx.kv("my-app-general_posts_preview")?;
            kv.put(&(now + "-" + &username), &new_post_string)?
                .execute()
                .await?;

            // * Set response headers
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                "https://cf-social-media-frontend.pages.dev",
            )?;
            Headers::set(headers, "Access-Control-Allow-Credentials", "true")?;
            Headers::set(
                headers,
                "Access-Control-Allow-Methods",
                "GET,HEAD,POST,OPTIONS",
            )?;
            Headers::set(headers, "Access-Control-Allow-Headers", "Content-Type")?;
            Ok(res)
        })
        .options_async("/posts", |_, _| async {
            let mut res = Response::ok("success")?;
            let headers = Response::headers_mut(&mut res);
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                "https://cf-social-media-frontend.pages.dev",
            )?;
            Ok(res)
        })
        .post_async("/updatelikes", |mut req, ctx| async move {
            // get value <username>-<time>
            let mut new_post: Value = req.json::<serde_json::Value>().await?;
            let kv = ctx.kv("my-app-general_posts_preview")?;
            let new_post_obj = new_post.as_object_mut().unwrap();
            let mut username = new_post_obj.get("username").unwrap().to_string();
            username.pop();
            username.remove(0);
            let mut time = new_post_obj.get("time").unwrap().to_string();
            time.pop();
            time.remove(0);
            let key = time + "-" + &username;
            kv.delete(&key).await?;
            let new_post_string = new_post.to_string();
            kv.put(&key, new_post_string)?.execute().await?;
            let mut res = Response::ok(format!("{}", new_post))?;
            let headers = Response::headers_mut(&mut res);
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                "https://cf-social-media-frontend.pages.dev",
            )?;
            Headers::set(
                headers,
                "Access-Control-Allow-Methods",
                "GET,HEAD,POST,OPTIONS",
            )?;
            Headers::set(headers, "Access-Control-Allow-Headers", "Content-Type")?;
            Ok(res)
        })
        .run(req, env)
        .await
}
