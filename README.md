# ETag middleware for Actix web

To use 
```rust
use actix_web::{web, App, HttpServer, HttpResponse, Error};
use actix_middleware_etag::{Etag};
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move ||
            App::new()
            // Add etag headers to your actix application. Calculating the hash of your GET bodies and putting the base64 hash in the ETag header
            .wrap(Etag::default())
                ...
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}
```

This will hash all bodies for GET requests and base64 encode the hash as a weak ETag header in the response
