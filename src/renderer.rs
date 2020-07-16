use crate::client::EnhancedSession;
use colored::*;
use confluence::rpser::xml;
use confluence::AttachmentRequest;
use confluence::{Error as ConfluenceError, Page, PageSummary, Session, UpdatePage};
use futures::future;
use mdbook::book::Chapter;
use mdbook::errors::Error as MdBookError;
use mdbook::renderer::RenderContext;
use mdbook::utils::new_cmark_parser;
use mdbook::BookItem;
use mime_guess::MimeGuess;
use pulldown_cmark::{Event, Tag};
use pulldown_cmark_to_cmark::fmt::cmark;
use regex::Regex;
use semver::Version;
use std::ffi::OsStr;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

pub static RENDERER_NAME: &str = "confluence";

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ConfluenceConfig {
    pub enabled: bool,
    pub url: String,
    pub username: String,
    pub password: String,
    pub title_prefix: Option<String>,
    pub root_page: i64,
}

impl ConfluenceConfig {
    fn chapter_title(&self, chapter: &Chapter) -> String {
        format!(
            "{}{}",
            self.title_prefix.as_deref().unwrap_or(""),
            chapter.name
        )
    }
}

pub struct ConfluenceRenderer {
    internal: Arc<InternalRenderer>,
}

trait AyncRenderer {
    fn render_group(
        self,
        items: Vec<BookItem>,
        parent_page: ParentPage,
        root_path: Arc<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>>;

    fn render_page(
        self,
        chapter: Chapter,
        parent: Arc<ParentPage>,
        existing_page_id: Option<i64>,
        root_path: Arc<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<String, Error>>>>;
}

struct InternalRenderer {
    session: Session,
    server_version: Version,
    config: ConfluenceConfig,
}

impl ConfluenceRenderer {
    pub async fn new(config: ConfluenceConfig) -> Result<ConfluenceRenderer, Error> {
        let session = Session::login(
            &config.url.clone(),
            &config.username.clone(),
            &config.password.clone(),
        )
        .await?;

        let server_version = session.get_server_version().await?;

        info!("Logged into Confluence. Version: {}", server_version);

        Ok(ConfluenceRenderer {
            internal: Arc::new(InternalRenderer {
                session,
                server_version,
                config,
            }),
        })
    }

    pub async fn render(&self, ctx: RenderContext) -> Result<(), Error> {
        let parent_page = self
            .internal
            .session
            .get_page_by_id(self.config().root_page)
            .await?;

        self.internal
            .clone()
            .render_group(
                ctx.book.sections,
                parent_page.into(),
                Arc::new(ctx.root.join(ctx.config.book.src)),
            )
            .await
    }

    pub async fn logout(self) -> Result<bool, Error> {
        #[allow(clippy::match_wild_err_arm)]
        match Arc::try_unwrap(self.internal) {
            Ok(internal) => internal.session.logout().await.map_err(Into::into),
            Err(_) => panic!("We should be done with our internal renderer"),
        }
    }

    fn config(&self) -> &ConfluenceConfig {
        &self.internal.config
    }
}

impl InternalRenderer {
    /// finds the existing page if we have one, or creates one without any content
    async fn get_existing_page(
        &self,
        chapter: &Chapter,
        existing_page_id: Option<i64>,
        parent: &ParentPage,
    ) -> Result<Page, Error> {
        match existing_page_id {
            None => {
                let new_page = UpdatePage {
                    id: None,
                    space: parent.space.clone(),
                    title: self.config.chapter_title(chapter),
                    content: "".into(),
                    version: None,
                    parent_id: Some(parent.id),
                };

                self.session.store_page(new_page).await.map_err(Into::into)
            }
            Some(id) => self.session.get_page_by_id(id).await.map_err(Into::into),
        }
    }

    async fn create_page_content(
        &self,
        chapter: &Chapter,
        existing_page: Page,
        parent: &ParentPage,
        root_path: &PathBuf,
    ) -> Result<UpdatePage, Error> {
        let mut events = vec![];
        let mut last_image = None;
        let mut chapter_path = chapter.path.clone();
        chapter_path.pop();
        chapter_path = root_path.join(&chapter_path);

        for event in new_cmark_parser(&chapter.content) {
            last_image = match (event, last_image) {
                (Event::Start(Tag::Image(link_type, url, title)), None) => {
                    match self
                        .upload_image(
                            title.to_string(),
                            url.to_string(),
                            &chapter_path,
                            existing_page.id,
                        )
                        .await
                    {
                        Some(new_url) => {
                            // if we have a new url update our tags
                            let tag = Tag::Image(link_type, new_url.into(), title);
                            events.push(Event::Start(tag.clone()));
                            Some(tag)
                        }
                        None => {
                            events.push(Event::Start(Tag::Image(link_type, url, title)));
                            None
                        }
                    }
                }
                (Event::End(Tag::Image(_, _, _)), Some(last)) => {
                    events.push(Event::End(last));
                    None
                }
                (e, last) => {
                    events.push(e);
                    last
                }
            }
        }

        let mut buf = String::with_capacity(chapter.content.len());
        cmark(events.iter(), &mut buf, None)
            .map_err(|err| Error::Error(format!("Markdown serialization failed: {}", err)))?;

        Ok(UpdatePage {
            id: Some(existing_page.id),
            space: parent.space.clone(),
            title: self.config.chapter_title(chapter),
            content: self.to_page_content(&buf),
            version: Some(existing_page.version),
            parent_id: Some(parent.id),
        })
    }

    async fn upload_image(
        &self,
        title: String,
        image_url: String,
        root_path: &PathBuf,
        page_id: i64,
    ) -> Option<String> {
        lazy_static! {
            // borrowed from mdbook
            static ref SCHEME_LINK: Regex = Regex::new(r"^[a-z][a-z0-9+.-]*:").unwrap();
        }

        // check for scheme: links and don't modify them
        if !SCHEME_LINK.is_match(&image_url) {
            info!("Attempting to upload file: {}", image_url);
            // try to find our file to upload on disk
            let path = root_path.join(image_url);
            let result = self
                .session
                .add_file(
                    page_id,
                    AttachmentRequest::new(
                        path.file_name().and_then(OsStr::to_str).unwrap_or(""),
                        MimeGuess::from_path(&path).first_or_octet_stream(),
                        title,
                        None,
                    ),
                    &path,
                )
                .await
                .map(|a| match a.url {
                    Some(file_url) => {
                        info!("{} file at: {}", "Uploaded".green(), file_url);
                        Some(file_url)
                    }
                    None => {
                        error!("Uploaded an attachment but couldn't find a url for it");
                        None
                    }
                });

            match result {
                Ok(url) => url,
                Err(e) => {
                    error!("Attempted to upload file but hit an error: {:?}", e);
                    None
                }
            }
        } else {
            None
        }
    }

    fn to_page_content(&self, markdown: &str) -> String {
        format!(
            r#"<ac:structured-macro ac:name="markdown" ac:schema-version="1" ac:macro-id="249327eb-2c99-42ca-a7a7-487e1c0c7e04">
           <ac:plain-text-body>{}</ac:plain-text-body>
        </ac:structured-macro>"#,
            self.to_cdata(markdown)
        )
    }

    /// https://stackoverflow.com/questions/223652/is-there-a-way-to-escape-a-cdata-end-token-in-xml
    fn to_cdata(&self, s: &str) -> String {
        // https://confluence.atlassian.com/confkb/saving-page-throws-unable-to-communicate-with-server-message-921470725.html
        // confluence versions under 7.3 don't support graphemes with more than 4 bytes. So we change them to '⸮'
        let s = if !self.supports_emoji() {
            UnicodeSegmentation::graphemes(s, true)
                .flat_map(|g| {
                    if g.len() >= 4 {
                        warn!("Removed unsupported char: {}", g);
                        "⸮".chars()
                    } else {
                        g.chars()
                    }
                })
                .collect()
        } else {
            s.to_string()
        };

        let escaped = s
            .split("]]>")
            .collect::<Vec<&str>>()
            .join("]]]]><![CDATA[>");

        format!("<![CDATA[{}]]>", escaped)
    }

    fn supports_emoji(&self) -> bool {
        self.server_version >= Version::parse("7.3.0").unwrap()
    }
}

impl AyncRenderer for Arc<InternalRenderer> {
    fn render_group(
        self,
        items: Vec<BookItem>,
        parent_page: ParentPage,
        root_path: Arc<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
        Box::pin(async move {
            let mut children = self.session.get_children(parent_page.id).await?;

            let parent_page = Arc::new(parent_page);
            let mut child_futures = vec![];

            for item in items.into_iter() {
                if let BookItem::Chapter(chapter) = item {
                    let mut existing_page_id = None;
                    for i in (0..children.len()).rev() {
                        if children[i].title == self.config.chapter_title(&chapter) {
                            // we need to get the page version number so grab the id
                            existing_page_id = Some(children.remove(i).id);
                            break;
                        }
                    }

                    child_futures.push(self.clone().render_page(
                        chapter,
                        parent_page.clone(),
                        existing_page_id,
                        root_path.clone(),
                    ));
                }
            }

            // join our child futures and render the results
            for result in future::join_all(child_futures).await {
                match result {
                    Ok(success) => info!("{}", success),
                    Err(e) => error!("{}", e),
                }
            }

            // any remaining children were probably deleted from the book so delete them here
            for deleted_child in children {
                let deleted_id = deleted_child.id;
                match self.session.remove_page(deleted_id).await {
                    Ok(_) => info!(
                        "{} page: '{}' {}",
                        "Deleted".red(),
                        deleted_child.title,
                        deleted_child.url
                    ),
                    Err(e) => error!("{:?}", e),
                }
            }

            Ok(())
        })
    }

    fn render_page(
        self,
        chapter: Chapter,
        parent: Arc<ParentPage>,
        existing_page_id: Option<i64>,
        root_path: Arc<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<String, Error>>>> {
        Box::pin(async move {
            let existing_page = self
                .get_existing_page(&chapter, existing_page_id, &parent)
                .await?;

            let new_page = self
                .create_page_content(&chapter, existing_page, &parent, &root_path)
                .await?;
            let new_page = self.session.store_page(new_page).await?;
            let success = format!(
                "{} '{}' {}",
                if existing_page_id.is_some() {
                    "Updated".yellow()
                } else {
                    "Created".green()
                },
                new_page.title,
                new_page.url
            );

            self.render_group(chapter.sub_items, new_page.into(), root_path.clone())
                .await?;

            Ok(success)
        })
    }
}

#[derive(Debug)]
pub enum Error {
    Confluence(ConfluenceError),
    MdBook(MdBookError),
    Error(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Confluence(e) => write!(f, "Failed to update Confluence: {:?}", e),
            Error::MdBook(e) => write!(f, "MdBook Error: {:?}", e),
            Error::Error(e) => write!(f, "{}", e),
        }
    }
}

impl From<confluence::Error> for Error {
    fn from(e: ConfluenceError) -> Self {
        Error::Confluence(e)
    }
}

impl From<xml::Error> for Error {
    fn from(e: xml::Error) -> Self {
        confluence::Error::from(e).into()
    }
}

impl From<MdBookError> for Error {
    fn from(e: MdBookError) -> Self {
        Error::MdBook(e)
    }
}

#[derive(Debug, Clone)]
struct ParentPage {
    id: i64,
    space: String,
}

impl From<Page> for ParentPage {
    fn from(page: Page) -> Self {
        ParentPage {
            id: page.id,
            space: page.space,
        }
    }
}

impl From<PageSummary> for ParentPage {
    fn from(page: PageSummary) -> Self {
        ParentPage {
            id: page.id,
            space: page.space,
        }
    }
}
