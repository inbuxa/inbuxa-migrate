/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::dav::href::Href;

pub const NS_DAV: &str = "DAV:";
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const NS_APPLE_ICAL: &str = "http://apple.com/ns/ical/";

pub fn propfind_current_user_principal() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\">\
         <d:prop><d:current-user-principal/></d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_calendar_home_set() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
         <d:prop><c:calendar-home-set/></d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_addressbook_home_set() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
         <d:prop><c:addressbook-home-set/></d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_principal_and_calendar_home_set() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
         <d:prop>\
         <d:current-user-principal/>\
         <c:calendar-home-set/>\
         </d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_principal_and_addressbook_home_set() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
         <d:prop>\
         <d:current-user-principal/>\
         <c:addressbook-home-set/>\
         </d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_calendar_collections() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind \
           xmlns:d=\"DAV:\" \
           xmlns:c=\"urn:ietf:params:xml:ns:caldav\" \
           xmlns:ic=\"http://apple.com/ns/ical/\">\
         <d:prop>\
         <d:resourcetype/>\
         <d:displayname/>\
         <c:calendar-description/>\
         <c:supported-calendar-component-set/>\
         <c:calendar-timezone/>\
         <ic:calendar-color/>\
         <ic:calendar-order/>\
         </d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_addressbook_collections() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind \
           xmlns:d=\"DAV:\" \
           xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
         <d:prop>\
         <d:resourcetype/>\
         <d:displayname/>\
         <c:addressbook-description/>\
         </d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_webdav_listing() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\">\
         <d:prop>\
         <d:resourcetype/>\
         <d:displayname/>\
         <d:getetag/>\
         <d:getcontenttype/>\
         <d:getlastmodified/>\
         <d:getcontentlength/>\
         <d:creationdate/>\
         </d:prop>\
         </d:propfind>",
    )
}

pub fn propfind_dav_items() -> String {
    String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:propfind xmlns:d=\"DAV:\">\
         <d:prop>\
         <d:resourcetype/>\
         <d:getetag/>\
         <d:getcontenttype/>\
         <d:getlastmodified/>\
         </d:prop>\
         </d:propfind>",
    )
}

pub fn calendar_multiget(hrefs: &[Href]) -> String {
    let mut out = String::with_capacity(256 + hrefs.len() * 64);
    out.push_str(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <c:calendar-multiget \
           xmlns:d=\"DAV:\" \
           xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
         <d:prop>\
         <d:getetag/>\
         <c:calendar-data/>\
         </d:prop>",
    );
    for h in hrefs {
        write_href_element(&mut out, h);
    }
    out.push_str("</c:calendar-multiget>");
    out
}

pub fn addressbook_multiget(hrefs: &[Href]) -> String {
    let mut out = String::with_capacity(256 + hrefs.len() * 64);
    out.push_str(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <c:addressbook-multiget \
           xmlns:d=\"DAV:\" \
           xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
         <d:prop>\
         <d:getetag/>\
         <c:address-data/>\
         </d:prop>",
    );
    for h in hrefs {
        write_href_element(&mut out, h);
    }
    out.push_str("</c:addressbook-multiget>");
    out
}

/// The properties a new calendar is made with (RFC 4791, section 5.3.1).
pub fn mkcalendar(name: &str, description: Option<&str>, color: Option<&str>) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <c:mkcalendar xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" \
           xmlns:a=\"http://apple.com/ns/ical/\">\
         <d:set><d:prop>",
    );
    push_element(&mut out, "d:displayname", name);
    if let Some(description) = description {
        push_element(&mut out, "c:calendar-description", description);
    }
    if let Some(color) = color {
        push_element(&mut out, "a:calendar-color", color);
    }
    out.push_str("</d:prop></d:set></c:mkcalendar>");
    out
}

/// An extended MKCOL that makes an address book (RFC 5689, RFC 6352).
pub fn mkcol_addressbook(name: &str, description: Option<&str>) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <d:mkcol xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
         <d:set><d:prop>\
         <d:resourcetype><d:collection/><c:addressbook/></d:resourcetype>",
    );
    push_element(&mut out, "d:displayname", name);
    if let Some(description) = description {
        push_element(&mut out, "c:addressbook-description", description);
    }
    out.push_str("</d:prop></d:set></d:mkcol>");
    out
}

fn push_element(out: &mut String, tag: &str, text: &str) {
    out.push('<');
    out.push_str(tag);
    out.push('>');
    write_escaped(out, text);
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

fn write_href_element(out: &mut String, h: &Href) {
    out.push_str("<d:href>");
    write_escaped(out, h.as_str());
    out.push_str("</d:href>");
}

fn write_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dav::href::Href;

    #[test]
    fn principal_body_matches_expected_shape() {
        let body = propfind_current_user_principal();
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<d:current-user-principal/>"));
        assert!(body.contains("xmlns:d=\"DAV:\""));
    }

    #[test]
    fn calendar_home_set_includes_caldav_namespace() {
        let body = propfind_calendar_home_set();
        assert!(body.contains("xmlns:c=\"urn:ietf:params:xml:ns:caldav\""));
        assert!(body.contains("<c:calendar-home-set/>"));
    }

    #[test]
    fn addressbook_home_set_includes_carddav_namespace() {
        let body = propfind_addressbook_home_set();
        assert!(body.contains("xmlns:c=\"urn:ietf:params:xml:ns:carddav\""));
        assert!(body.contains("<c:addressbook-home-set/>"));
    }

    #[test]
    fn calendar_multiget_lists_hrefs_in_order() {
        let hrefs = vec![
            Href::from_normalised("/dav/cal/u/d/a.ics".to_owned()),
            Href::from_normalised("/dav/cal/u/d/b.ics".to_owned()),
        ];
        let body = calendar_multiget(&hrefs);
        let a_pos = body.find("/a.ics").unwrap();
        let b_pos = body.find("/b.ics").unwrap();
        assert!(a_pos < b_pos);
        assert!(body.contains("<c:calendar-data/>"));
    }

    #[test]
    fn addressbook_multiget_emits_address_data_element() {
        let hrefs = vec![Href::from_normalised("/dav/card/u/d/a.vcf".to_owned())];
        let body = addressbook_multiget(&hrefs);
        assert!(body.contains("<c:address-data/>"));
        assert!(body.contains("<d:href>/dav/card/u/d/a.vcf</d:href>"));
    }

    #[test]
    fn new_collections_carry_their_names_escaped() {
        let body = mkcalendar("Work & <home>", Some("Shifts"), Some("#FF0000FF"));
        assert!(body.contains("<d:displayname>Work &amp; &lt;home&gt;</d:displayname>"));
        assert!(body.contains("<c:calendar-description>Shifts</c:calendar-description>"));
        assert!(body.contains("<a:calendar-color>#FF0000FF</a:calendar-color>"));
        let body = mkcol_addressbook("Family", None);
        assert!(body.contains("<c:addressbook/></d:resourcetype>"));
        assert!(body.contains("<d:displayname>Family</d:displayname>"));
        assert!(!body.contains("addressbook-description"));
    }

    #[test]
    fn escape_handles_xml_metacharacters() {
        let h = Href::from_normalised("/dav/cal/u/d/&lt;weird&gt;.ics".to_owned());
        let body = calendar_multiget(&[h]);
        assert!(body.contains("&amp;lt;weird&amp;gt;.ics"));
    }

    #[test]
    fn webdav_listing_requests_filesystem_props() {
        let body = propfind_webdav_listing();
        assert!(body.contains("<d:getcontenttype/>"));
        assert!(body.contains("<d:getlastmodified/>"));
        assert!(body.contains("<d:getcontentlength/>"));
        assert!(body.contains("<d:creationdate/>"));
    }
}
