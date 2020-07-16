# mdBook Confluence

This syncs your mdbook to confluence if you have the [Markdown](https://scriptrunner.adaptavist.com/5.5.11/confluence/macros/BundledMacros.html#_markdown_macro)
macro installed.

```toml
[output.confluence]
enabled = true # this lets you disable the renderer until you need it
url = "https://your-confluence-url.com"
username = "username"
password = "for manual runs don't persist your password here"
title_prefix = "Some prefix to keep your page names unique"
root_page = 1234 # the page id of where to store your book
```

### 
* `$(read -s -p "Enter your password: " password && MDBOOK_output__password="${password}" mdbook build)`