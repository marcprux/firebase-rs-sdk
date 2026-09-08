# Firebase Storage

This module ports core pieces of the Firebase Storage Web SDK to Rust so applications
can discover buckets, navigate object paths, and perform common download, metadata,
and upload operations using the SDK's shared async HTTP client, which works on native and wasm targets.

It provides functionality to interact with Firebase Storage, including
uploading and downloading files, managing metadata, and handling storage references.

It includes error handling, configuration options, and integration with Firebase apps.

Coverage: see the [table in the repository README](https://github.com/marcprux/firebase-rs-sdk#coverage), generated from `docs/coverage.toml`.

## Features:

- Connect to Firebase Storage emulator
- Get storage instance for a Firebase app
- Register storage component
- Manage storage references
- Handle file uploads with progress tracking, pause, resume and cancel
- Upload strings and browser blobs with shared helpers
- Stream large uploads directly from async readers
- Stream downloads as native async readers (non-WASM)
- List files and directories in storage, one page at a time or all at once
- Manage object metadata
- Comprehensive error handling

## Quick Start Example

```rust,ignore
use firebase_core::app::*;
use firebase_storage::*;

#[tokio::main]
async fn main() -> StorageResult<()> {
    let options = FirebaseOptions {
        storage_bucket: Some("BUCKET_NAME".into()),
        ..Default::default()
    };

    let app = initialize_app(options, Some(FirebaseAppSettings::default())).await?;

    let storage = get_storage_for_app(Some(app), None).await?;

    let photos = storage
        .root_reference()?
        .child("photos");

    // Upload a photo; small payloads are sent via multipart upload while larger blobs use the resumable API.
    let image_bytes = vec![/* PNG bytes */];
    let mut upload_metadata = UploadMetadata::new().with_content_type("image/png");
    upload_metadata.insert_custom_metadata("uploaded-by", "quickstart");

    let metadata = photos
        .child("welcome.png")
        .upload_bytes(image_bytes, Some(upload_metadata))
        .await?;
    println!(
        "Uploaded {} to bucket {}",
        metadata.name.unwrap_or_default(),
        metadata.bucket.unwrap_or_default()
    );

    // List the directory and stream the first few kilobytes of each item.
    let listing = photos.list_all().await?;
    for object in listing.items {
        let url = object.get_download_url().await?;
        let bytes = object.get_bytes(Some(256 * 1024)).await?;
        println!("{} -> {} bytes", url, bytes.len());
    }

    Ok(())
}
```

## Resumable Uploads

Payloads larger than 256 KiB are uploaded chunk by chunk through the resumable protocol.
`upload_bytes_resumable` hands back a task that can be observed and controlled while it runs:
cancelling discards the session server-side, so no half-written object is left behind.

```rust,ignore
use firebase_storage::{StorageReference, UploadTaskState};

async fn upload(reference: StorageReference, bytes: Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
    let task = reference.upload_bytes_resumable(bytes, None)?;
    let handle = task.handle();

    let unsubscribe = handle.on_state_changed(|snapshot| {
        println!(
            "{}: {} of {} bytes",
            snapshot.state, snapshot.bytes_transferred, snapshot.total_bytes
        );
    });

    // `handle` is `Send + Sync + Clone`, so it can pause(), resume() or cancel() the upload from
    // another task, from a UI callback, or from inside the observer above.
    let metadata = task.run_to_completion().await?;
    unsubscribe();

    assert_eq!(handle.state(), UploadTaskState::Completed);
    println!("uploaded {:?}", metadata.name);
    Ok(())
}
```

A cancelled upload fails with `storage/canceled`. Pausing stops the task at the next chunk
boundary; `UploadTask::refresh_status` asks the server how many bytes it stored before resuming.

## References to the Firebase JS SDK

- QuickStart: <https://firebase.google.com/docs/storage/web/start>
- API: <https://firebase.google.com/docs/reference/js/storage.md#storage_package>
- Github Repo - Module: <https://github.com/firebase/firebase-js-sdk/tree/master/packages/storage>
- Github Repo - API: <https://github.com/firebase/firebase-js-sdk/tree/main/packages/firebase/storage>



