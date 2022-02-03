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
        .get_async("/posts", |_req, ctx| async move {
            // * Get the kv
            let kv = ctx.kv("my-app-general_posts_preview")?;

            // * Get a list of keys
            let keys = kv.list().execute().await?.keys;
            let mut posts: Vec<Value> = vec![];

            for key in keys {
                // let value = kv.get(&key.name).await.unwrap().unwrap().as_string();
                let value = match kv.get(&key.name).await {
                    Ok(r) => match r {
                        Some(val) => val.as_string(),
                        None => return Response::error("No value found for key", 502),
                    },
                    Err(e) => {
                        return Response::error(format!("Could not get value for key. Error: {}", e), 502)
                    }
                };
                
                // * Convert string value to a json and push on to posts vector
                let value_json = json!(value);
                posts.push(value_json);
            }
            
            // * Create OK response and set response headers
            let mut res = Response::from_json(&posts)?;
            let headers = Response::headers_mut(&mut res);
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                &ctx.var("FRONTEND_URL")?.to_string(),
            )?;
            Headers::set(headers, "Access-Control-Allow-Credentials", "true")?;
            Headers::set(headers, "transfer-encoding", "chunked")?;
            Headers::set(headers, "vary", "Accept-Encoding")?;
            Headers::set(headers, "connection", "keep-alive")?;
            Ok(res)
        })
        .post_async("/posts", |mut req, ctx| async move {
            // * Get the new post
            let mut new_post: Value = req.json::<serde_json::Value>().await?;

            // * Get the current time and set it in the post to the "time" field
            let now = Utc::now().to_rfc3339().to_string();
            *new_post.get_mut("time").unwrap() = serde_json::Value::String(now.clone());

            // * Get the cookie header if present, otherwise set the cookie to empty string
            let req_cookie = req.headers().get("Cookie")?.unwrap_or("".to_string());

            // * Get username and remove double quotes from name
            let mut username = match new_post.get("username") {
                Some(n) => n.to_string(),
                None => return Response::error("No username present in new post", 400),
            };
            username.pop();
            username.remove(0);

            // * Create the response and get the headers
            let mut res = Response::ok(format!("{}", new_post))?;
            let headers = Response::headers_mut(&mut res);

            // * Get a vector of the users from users namespace
            let users = crate::check_user(&ctx).await?;

            // * Check if this is an existing user
            if users.contains(&username) {
                if req_cookie.len() > 0 {
                    // * Send a request to the authentication server at endpoint /verify
                    let client = reqwest::Client::new();
                    let auth_resp = match client
                        .get(format!(
                            "{}/verify",
                            ctx.var("AUTH_SERVER_URL")?.to_string()
                        ))
                        .header(COOKIE, req_cookie)
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            return Response::error(
                                format!("Could not verify user. Error: {}", e),
                                401,
                            )
                        }
                    };

                    let resp_body = match auth_resp.text().await {
                        Ok(r) => r,
                        Err(e) => {
                            return Response::error(
                                format!(
                                "Could not get response body from authentication server. Error: {}",
                                e
                            ),
                                502,
                            )
                        }
                    };
                    if resp_body != username {
                        return Response::error("Could not verify user", 401);
                    }
                }
            } else {
                // * Add new user to users KV
                crate::add_user(&username, &now, &ctx).await;

                // * Get the set-cookie header from authorization server and forward it to the response
                let auth_resp = reqwest::get(format!(
                    "{}/auth/{}",
                    ctx.var("AUTH_SERVER_URL")?.to_string(),
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
                &ctx.var("FRONTEND_URL")?.to_string(),
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
        .options_async("/posts", |_, ctx| async move {
            // * For preflight response
            let mut res = Response::ok("success")?;
            let headers = Response::headers_mut(&mut res);
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                &ctx.var("FRONTEND_URL")?.to_string(),
            )?;
            Ok(res)
        })
        .post_async("/updatelikes", |mut req, ctx| async move {
            // * Get the post to like and conver to a mutable object
            let mut post_to_like: Value = req.json::<serde_json::Value>().await?;
            let post_obj = post_to_like.as_object_mut().unwrap();

            // * Get username and time and remove double quotes
            let mut username = post_obj.get("username").unwrap().to_string();
            username.pop();
            username.remove(0);

            let mut time = post_obj.get("time").unwrap().to_string();
            time.pop();
            time.remove(0);

            // * Reconstruct the key to find th epost in the kv
            let key = time + "-" + &username;

            // * Replace the previous post with the updated post which contains an additional like/vote
            let kv = ctx.kv("my-app-general_posts_preview")?;
            kv.delete(&key).await?;
            let new_post_string = post_to_like.to_string();
            kv.put(&key, new_post_string)?.execute().await?;

            // * Create OK response and set response headers
            let mut res = Response::ok(format!("{}", post_to_like))?;
            let headers = Response::headers_mut(&mut res);
            Headers::set(
                headers,
                "Access-Control-Allow-Origin",
                &ctx.var("FRONTEND_URL")?.to_string(),
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
