use crate::renderer::Error;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use confluence::rpser::xml::{BuildElement, EnhancedNode, Error as XMLError};
use confluence::rpser::Method;
use confluence::{Error as ConfluenceError, Page, PageSummary, Session, UpdatePage};
use mime_guess::Mime;
use semver::Version;
use std::io::Read;
use std::sync::Arc;
use std::{io, mem};
use tokio::task;
use xmltree::{Element, XMLNode};

pub async fn login(url: String, username: String, password: String) -> Result<Arc<Session>, Error> {
    request(move || Session::login(&url, &username, &password).map(Arc::new))
        .await
        .map_err(Into::into)
}

#[async_trait]
pub trait AsyncSession: Sized {
    async fn get_server_version(&self) -> Result<Version, Error>;

    async fn get_page_by_id(&self, page_id: i64) -> Result<Page, Error>;

    async fn get_children(&self, page_id: i64) -> Result<Vec<PageSummary>, Error>;

    async fn store_page(&self, page: UpdatePage) -> Result<Page, Error>;

    async fn remove_page(&self, page_id: i64) -> Result<(), Error>;

    async fn add_attachment<R>(
        &self,
        page_id: i64,
        attachment: AttachmentRequest,
        mut data_reader: R,
    ) -> Result<AttachmentResponse, Error>
    where
        R: Read + Send;

    async fn request<F, R, E>(&self, f: F) -> Result<R, Error>
    where
        F: FnOnce(&Session) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: Into<Error> + Send + 'static;
}

#[async_trait]
impl AsyncSession for Arc<Session> {
    async fn get_server_version(&self) -> Result<Version, Error> {
        self.request(|session| {
            fn call_confluence(session: &Session) -> Result<(i32, i32), ConfluenceError> {
                let info = session
                    .call(
                        Method::new("getServerInfo")
                            .with(Element::node("token").with_text(session.token())),
                    )?
                    .body
                    .descend(&["getServerInfoReturn"])?
                    .into_element()?;

                let major_version = info
                    .get_at_path(&["majorVersion"])
                    .and_then(|e| e.as_int())?;
                let minor_version = info
                    .get_at_path(&["minorVersion"])
                    .and_then(|e| e.as_int())?;

                Ok((major_version, minor_version))
            }

            let (major_version, minor_version) = call_confluence(session)?;

            Version::parse(&format!("{}.{}.0", major_version, minor_version)).map_err(|error| {
                Error::Error(format!("Failed to parse Confluence Version: {}", error))
            })
        })
        .await
    }

    async fn get_page_by_id(&self, page_id: i64) -> Result<Page, Error> {
        self.request(move |s| s.get_page_by_id(page_id)).await
    }

    async fn get_children(&self, page_id: i64) -> Result<Vec<PageSummary>, Error> {
        self.request(move |s| s.get_children(page_id)).await
    }

    async fn store_page(&self, page: UpdatePage) -> Result<Page, Error> {
        self.request(|s| s.store_page(page)).await
    }

    async fn remove_page(&self, page_id: i64) -> Result<(), Error> {
        self.request(move |s| {
            s.call(
                Method::new("removePage")
                    .with(Element::node("token").with_text(s.token()))
                    .with(Element::node("pageId").with_text(page_id.to_string())),
            )
        })
        .await?;

        Ok(())
    }

    async fn add_attachment<R>(
        &self,
        page_id: i64,
        attachment: AttachmentRequest,
        mut data_reader: R,
    ) -> Result<AttachmentResponse, Error>
    where
        R: Read + Send,
    {
        let mut written_data = vec![];
        let mut writer = base64::write::EncoderWriter::new(&mut written_data, base64::STANDARD);
        io::copy(&mut data_reader, &mut writer).expect("Couldn't read file");
        mem::drop(writer);

        let data = String::from_utf8(written_data).expect("Base64 should always be valid UTF-8");

        let attachment = self
            .request(move |s| -> Result<AttachmentResponse, ConfluenceError> {
                let response = s.call(
                    Method::new("addAttachment")
                        .with(Element::node("token").with_text(s.token()))
                        .with(Element::node("contentId").with_text(page_id.to_string()))
                        .with(attachment.into())
                        .with(Element::node("attachmentData").with_text(data)),
                )?;

                let attachment = AttachmentResponse::from_node(
                    response.body.descend(&["addAttachmentReturn"])?,
                )?;

                Ok(attachment)
            })
            .await?;

        Ok(attachment)
    }

    async fn request<F, R, E>(&self, f: F) -> Result<R, Error>
    where
        F: FnOnce(&Session) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: Into<Error> + Send + 'static,
    {
        let self_ref = self.clone();

        request(move || f(&self_ref)).await.map_err(Into::into)
    }
}

pub struct AttachmentRequest {
    file_name: String,
    content_type: Mime,
    title: Option<String>,
    comment: Option<String>,
}

impl AttachmentRequest {
    pub fn new<N, T, C>(file_name: N, content_type: Mime, title: T, comment: C) -> AttachmentRequest
    where
        N: Into<String>,
        T: Into<Option<String>>,
        C: Into<Option<String>>,
    {
        AttachmentRequest {
            file_name: file_name.into(),
            content_type: content_type.into(),
            title: title.into(),
            comment: comment.into(),
        }
    }
}

impl Into<Element> for AttachmentRequest {
    fn into(self) -> Element {
        let mut children = vec![];

        children.push(Element::node("fileName").with_text(self.file_name));
        children.push(Element::node("contentType").with_text(format!("{}", self.content_type)));
        if let Some(title) = self.title {
            children.push(Element::node("title").with_text(title));
        }
        if let Some(comment) = self.comment {
            children.push(Element::node("comment").with_text(comment));
        }

        Element::node("attachment").with_children(children)
    }
}

pub struct AttachmentResponse {
    pub comment: Option<String>,
    pub content_type: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub creator: Option<String>,
    pub file_name: Option<String>,
    pub file_size: i64,
    pub id: i64,
    pub page_id: i64,
    pub title: Option<String>,
    pub url: Option<String>,
}

impl AttachmentResponse {
    fn from_node(node: XMLNode) -> Result<Self, XMLError> {
        if let XMLNode::Element(element) = node {
            Ok(AttachmentResponse {
                comment: element
                    .get_at_path(&["comment"])
                    .and_then(|e| e.as_string())
                    .ok(),
                content_type: element
                    .get_at_path(&["contentType"])
                    .and_then(|e| e.as_string())
                    .ok(),
                created: element
                    .get_at_path(&["created"])
                    .and_then(|e| e.as_datetime())
                    .ok(),
                creator: element
                    .get_at_path(&["creator"])
                    .and_then(|e| e.as_string())
                    .ok(),
                file_name: element
                    .get_at_path(&["fileName"])
                    .and_then(|e| e.as_string())
                    .ok(),
                file_size: element
                    .get_at_path(&["fileSize"])
                    .and_then(|e| e.as_long())?,
                id: element.get_at_path(&["id"]).and_then(|e| e.as_long())?,
                page_id: element.get_at_path(&["pageId"]).and_then(|e| e.as_long())?,
                title: element
                    .get_at_path(&["title"])
                    .and_then(|e| e.as_string())
                    .ok(),
                url: element
                    .get_at_path(&["url"])
                    .and_then(|e| e.as_string())
                    .ok(),
            })
        } else {
            Err(XMLError::ExpectedElement { found: node })
        }
    }
}

#[inline]
async fn request<F, R, E>(f: F) -> Result<R, E>
where
    F: FnOnce() -> Result<R, E> + Send + 'static,
    R: Send + 'static,
    E: Into<Error> + Send + 'static,
{
    task::spawn_blocking(f)
        .await
        .expect("Our task was cancelled")
}
