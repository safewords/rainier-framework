//! String inflection helpers, as free functions.
//!
//! These are load-bearing rather than cosmetic: resource routing derives a URI
//! from a controller name (`PostController` → `posts`), the console's `make:`
//! commands derive file names from class names, and route-model binding
//! derives a parameter name from an entity name. All of them need the same
//! small inflection vocabulary.

/// `"user_profile"` / `"user-profile"` / `"UserProfile"` → `"userProfile"`.
pub fn camel(value: &str) -> String {
    let studly = studly(value);
    let mut chars = studly.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `"user_profile"` / `"user-profile"` / `"user profile"` → `"UserProfile"`.
pub fn studly(value: &str) -> String {
    value
        .split(|c: char| c == '_' || c == '-' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// `"UserProfile"` → `"user_profile"`. Runs of capitals stay together, so
/// `"HTTPRequest"` becomes `"http_request"` rather than `"h_t_t_p_request"`.
pub fn snake(value: &str) -> String {
    delimit(value, '_')
}

/// `"UserProfile"` → `"user-profile"`.
pub fn kebab(value: &str) -> String {
    delimit(value, '-')
}

fn delimit(value: &str, delimiter: char) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    let chars: Vec<char> = value.chars().collect();

    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' || c.is_whitespace() {
            if !out.ends_with(delimiter) && !out.is_empty() {
                out.push(delimiter);
            }
            continue;
        }

        if c.is_uppercase() && i > 0 {
            let prev = chars[i - 1];
            // Break before a capital that starts a new word: either the
            // previous char was lowercase/numeric ("userProfile"), or we are at
            // the tail of a capital run followed by a lowercase ("HTTPRequest"
            // breaks before the R, not before the P).
            let ends_run = prev.is_lowercase()
                || prev.is_numeric()
                || chars.get(i + 1).is_some_and(|next| next.is_lowercase());
            if ends_run && !out.is_empty() && !out.ends_with(delimiter) {
                out.push(delimiter);
            }
        }

        out.extend(c.to_lowercase());
    }

    out
}

/// A naive English pluraliser, covering the endings that show up in table and
/// resource names. Wrong for genuinely irregular nouns outside its short table —
/// name the resource explicitly when it matters.
///
/// Pluralising is **idempotent**: `plural("posts") == "posts"`. Resource
/// routing feeds it names that may already be plural, and a `postses` table
/// would be a silent, annoying bug.
pub fn plural(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if is_plural(value) {
        return value.to_string();
    }
    pluralize_raw(value)
}

/// The inverse of [`plural`], to the same approximate standard. Also
/// idempotent: `singular("post") == "post"`.
pub fn singular(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    singularize_raw(value)
}

/// Whether `value` already reads as the plural of its own singular. Derived
/// rather than listed: singularise, re-pluralise, and see if we came back to
/// where we started.
fn is_plural(value: &str) -> bool {
    let singular = singularize_raw(value);
    singular != value && pluralize_raw(&singular).eq_ignore_ascii_case(value)
}

/// Pluralise unconditionally — no "is it already plural?" guard, so the two
/// raw functions can call each other without recursing.
fn pluralize_raw(value: &str) -> String {
    let lower = value.to_lowercase();
    if UNCOUNTABLE.contains(&lower.as_str()) {
        return value.to_string();
    }
    for (singular, plural) in IRREGULAR {
        if lower == *singular {
            return match_case(value, plural);
        }
    }

    let ends_with = |suffix: &str| lower.ends_with(suffix);

    if ends_with("s") || ends_with("x") || ends_with("z") || ends_with("ch") || ends_with("sh") {
        return format!("{value}es");
    }
    if ends_with("y") && !ends_with_vowel_before_y(&lower) {
        return format!("{}ies", &value[..value.len() - 1]);
    }
    // `-fe` before `-f`: "knife" is `knif|e`, not `knif|f`. Both land on
    // `-ves`, and the words where that is wrong (knife/life/wife) are in
    // IRREGULAR above, which is checked first.
    if ends_with("fe") {
        return format!("{}ves", &value[..value.len() - 2]);
    }
    if ends_with("f") {
        return format!("{}ves", &value[..value.len() - 1]);
    }
    format!("{value}s")
}

/// Singularise unconditionally. See [`pluralize_raw`].
fn singularize_raw(value: &str) -> String {
    let lower = value.to_lowercase();
    if UNCOUNTABLE.contains(&lower.as_str()) {
        return value.to_string();
    }
    for (singular, plural) in IRREGULAR {
        if lower == *plural {
            return match_case(value, singular);
        }
        if lower == *singular {
            return value.to_string();
        }
    }

    if lower.ends_with("ies") && value.len() > 3 {
        return format!("{}y", &value[..value.len() - 3]);
    }
    // `-ves` is ambiguous ("wolves" → wolf, "knives" → knife). Default to the
    // bare `-f`; the `-fe` words are listed in IRREGULAR and matched above.
    if lower.ends_with("ves") && value.len() > 3 {
        return format!("{}f", &value[..value.len() - 3]);
    }
    for suffix in ["ses", "xes", "zes", "ches", "shes"] {
        if lower.ends_with(suffix) {
            return value[..value.len() - 2].to_string();
        }
    }
    if lower.ends_with('s') && !lower.ends_with("ss") {
        return value[..value.len() - 1].to_string();
    }
    value.to_string()
}

fn ends_with_vowel_before_y(lower: &str) -> bool {
    let mut chars = lower.chars().rev();
    chars.next(); // the 'y'
    matches!(chars.next(), Some('a' | 'e' | 'i' | 'o' | 'u'))
}

/// Copy the capitalisation style of `source` onto `replacement`, so
/// `plural("Person")` is `"People"` rather than `"people"`.
fn match_case(source: &str, replacement: &str) -> String {
    if source.chars().all(|c| c.is_uppercase() || !c.is_alphabetic()) {
        return replacement.to_uppercase();
    }
    if source.chars().next().is_some_and(char::is_uppercase) {
        let mut chars = replacement.chars();
        return match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        };
    }
    replacement.to_string()
}

/// `(singular, plural)` pairs the suffix rules cannot derive. Consulted before
/// the rules in both directions.
const IRREGULAR: &[(&str, &str)] = &[
    ("person", "people"),
    ("man", "men"),
    ("woman", "women"),
    ("child", "children"),
    ("tooth", "teeth"),
    ("foot", "feet"),
    ("mouse", "mice"),
    ("goose", "geese"),
    // `-fe` nouns: the generic `-ves` rule cannot tell these from wolf/leaf.
    ("knife", "knives"),
    ("life", "lives"),
    ("wife", "wives"),
];

const UNCOUNTABLE: &[&str] = &["equipment", "information", "money", "series", "species", "data"];

/// `"Hello, World!"` → `"hello-world"`. Non-alphanumerics collapse to a single
/// separator and the ends are trimmed.
pub fn slug(value: &str) -> String {
    slug_with(value, '-')
}

/// [`slug`] with a caller-chosen separator.
pub fn slug_with(value: &str, separator: char) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with(separator) {
            out.push(separator);
        }
    }
    out.trim_matches(separator).to_string()
}

/// Uppercase the first character, leaving the rest alone.
pub fn ucfirst(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `"user_profile"` → `"User profile"` — for human-readable validation
/// messages about a field.
pub fn humanize(value: &str) -> String {
    ucfirst(&snake(value).replace('_', " "))
}

/// Strip the module path from a fully-qualified Rust type name:
/// `"app::models::Post"` → `"Post"`. Used to derive defaults from
/// [`std::any::type_name`].
pub fn class_basename(value: &str) -> &str {
    // Generic parameters can contain `::` too, so only look at the head.
    let head = value.split('<').next().unwrap_or(value);
    head.rsplit("::").next().unwrap_or(head)
}

/// Cut `value` to at most `limit` characters, appending `end` when it had to
/// cut. Character-based, so it never splits a multi-byte char.
pub fn limit(value: &str, limit: usize, end: &str) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let truncated: String = value.chars().take(limit).collect();
    format!("{truncated}{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_conversions() {
        assert_eq!(studly("user_profile"), "UserProfile");
        assert_eq!(studly("user-profile"), "UserProfile");
        assert_eq!(camel("user_profile"), "userProfile");
        assert_eq!(snake("UserProfile"), "user_profile");
        assert_eq!(kebab("UserProfile"), "user-profile");
        assert_eq!(snake("user_profile"), "user_profile");
    }

    #[test]
    fn capital_runs_stay_together() {
        assert_eq!(snake("HTTPRequest"), "http_request");
        assert_eq!(snake("ParseJSON"), "parse_json");
        assert_eq!(kebab("APIToken"), "api-token");
    }

    #[test]
    fn pluralisation_covers_the_common_endings() {
        assert_eq!(plural("post"), "posts");
        assert_eq!(plural("class"), "classes");
        assert_eq!(plural("box"), "boxes");
        assert_eq!(plural("category"), "categories");
        assert_eq!(plural("day"), "days");
        assert_eq!(plural("knife"), "knives");
        assert_eq!(plural("person"), "people");
        assert_eq!(plural("Person"), "People");
        assert_eq!(plural("data"), "data");
    }

    #[test]
    fn pluralising_an_already_plural_word_is_a_no_op() {
        for word in ["posts", "classes", "boxes", "categories", "people", "knives", "days"] {
            assert_eq!(plural(word), word, "plural({word})");
        }
    }

    #[test]
    fn singularisation_is_the_inverse() {
        assert_eq!(singular("posts"), "post");
        assert_eq!(singular("classes"), "class");
        assert_eq!(singular("categories"), "category");
        assert_eq!(singular("people"), "person");
        assert_eq!(singular("knives"), "knife");
        assert_eq!(singular("post"), "post");
    }

    #[test]
    fn slugs_collapse_punctuation() {
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("  spaced   out  "), "spaced-out");
        assert_eq!(slug_with("Hello World", '_'), "hello_world");
    }

    #[test]
    fn basenames_drop_the_module_path() {
        assert_eq!(class_basename("app::models::Post"), "Post");
        assert_eq!(class_basename("Post"), "Post");
        assert_eq!(class_basename("core::option::Option<app::Post>"), "Option");
    }

    #[test]
    fn limit_never_splits_a_char() {
        assert_eq!(limit("hello", 10, "..."), "hello");
        assert_eq!(limit("hello world", 5, "..."), "hello...");
        assert_eq!(limit("héllo", 2, ""), "hé");
    }

    #[test]
    fn humanize_reads_as_a_sentence() {
        assert_eq!(humanize("email_address"), "Email address");
        assert_eq!(humanize("emailAddress"), "Email address");
    }
}
