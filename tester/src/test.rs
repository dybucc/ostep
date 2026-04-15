#[derive(Debug)]
pub(crate) struct Test {
    pub(crate) num: usize,
    pub(crate) out: Option<String>,
    pub(crate) err: Option<String>,
    pub(crate) rc: Option<String>,
    pub(crate) run: Option<String>,
    pub(crate) desc: Option<String>,
    pub(crate) pre: Option<String>,
    pub(crate) post: Option<String>,
}

impl Default for Test {
    fn default() -> Self {
        Self {
            num: 1,
            out: Option::default(),
            err: Option::default(),
            rc: Option::default(),
            run: Option::default(),
            desc: Option::default(),
            pre: Option::default(),
            post: Option::default(),
        }
    }
}
