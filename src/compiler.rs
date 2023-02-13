use std::cell::BorrowMutError;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt::Display;

use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::draft::{DRAFT2019, DRAFT2020, DRAFT4, DRAFT6, DRAFT7};
use crate::root::Root;
use crate::roots::Roots;
use crate::util::*;
use crate::*;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Draft {
    V4,
    V6,
    V7,
    V2019_09,
    V2020_12,
}

#[derive(Default)]
pub struct Compiler {
    roots: Roots,
    decoders: HashMap<String, Decoder>,
    media_types: HashMap<String, MediaType>,
}

impl Compiler {
    pub fn register_url_loader(&mut self, scheme: &'static str, url_loader: Box<dyn UrlLoader>) {
        self.roots.loader.register(scheme, url_loader);
    }

    pub fn set_default_draft(&mut self, d: Draft) {
        self.roots.default_draft = match d {
            Draft::V4 => &DRAFT4,
            Draft::V6 => &DRAFT6,
            Draft::V7 => &DRAFT7,
            Draft::V2019_09 => &DRAFT2019,
            Draft::V2020_12 => &DRAFT2020,
        }
    }

    pub fn add_resource(&mut self, url: &str, json: Value) -> Result<bool, CompileError> {
        let url = Url::parse(url).map_err(|e| CompileError::LoadUrlError {
            url: url.to_owned(),
            src: e.into(),
        })?;
        self.roots.or_insert(url, json)
    }

    pub fn compile(
        &mut self,
        target: &mut Schemas,
        mut loc: String,
    ) -> Result<SchemaIndex, CompileError> {
        if loc.rfind('#').is_none() {
            loc.push('#');
        }

        let mut queue = VecDeque::new();
        queue.push_back(loc);
        let mut sch_index = None;
        while let Some(loc) = queue.front() {
            let (url, ptr) = split(loc);
            let url = Url::parse(url).map_err(|e| CompileError::LoadUrlError {
                url: url.to_owned(),
                src: e.into(),
            })?;
            self.roots.or_load(url.clone())?;
            let root = self.roots.get(&url).unwrap();
            let v = root
                .lookup_ptr(ptr)
                .map_err(|_| CompileError::InvalidJsonPointer(loc.clone()))?;
            let Some(v) = v else {
                return Err(CompileError::NotFound(loc.to_owned()));
            };

            let sch = self.compile_one(target, v, loc.to_owned(), root, &mut queue)?;
            let loc = queue
                .pop_front()
                .ok_or(CompileError::Bug("queue must be non-empty".into()))?;
            let index = target.insert(loc, sch);
            sch_index = sch_index.or(Some(index));
        }
        sch_index.ok_or(CompileError::Bug("schema_index must exist".into()))
    }

    fn compile_one(
        &self,
        schemas: &Schemas,
        v: &Value,
        loc: String,
        root: &Root,
        queue: &mut VecDeque<String>,
    ) -> Result<Schema, CompileError> {
        let mut s = Schema::new(loc.clone());
        let Value::Object(obj) = v else {
            return Ok(s);
        };

        // helpers --
        let load_usize = |pname| {
            if let Some(Value::Number(n)) = obj.get(pname) {
                n.as_u64().map(|n| n as usize)
            } else {
                None
            }
        };
        let load_num = |pname| {
            if let Some(Value::Number(n)) = obj.get(pname) {
                Some(n.clone())
            } else {
                None
            }
        };
        let to_strings = |v: &Value| {
            if let Value::Array(a) = v {
                a.iter()
                    .filter_map(|t| {
                        if let Value::String(t) = t {
                            Some(t.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                vec![]
            }
        };
        let load_schema = |pname, queue: &mut VecDeque<String>| {
            if obj.contains_key(pname) {
                Some(schemas.enqueue(queue, format!("{loc}/{}", escape(pname))))
            } else {
                None
            }
        };
        let load_schema_arr = |pname, queue: &mut VecDeque<String>| {
            if let Some(Value::Array(arr)) = obj.get(pname) {
                (0..arr.len())
                    .map(|i| schemas.enqueue(queue, format!("{loc}/{pname}/{i}")))
                    .collect()
            } else {
                Vec::new()
            }
        };
        let load_schema_map = |pname, queue: &mut VecDeque<String>| {
            if let Some(Value::Object(obj)) = obj.get(pname) {
                obj.keys()
                    .map(|k| {
                        (
                            k.clone(),
                            schemas.enqueue(queue, format!("{loc}/{pname}/{}", escape(k))),
                        )
                    })
                    .collect()
            } else {
                HashMap::new()
            }
        };

        // draft4 --
        if let Some(Value::String(ref_)) = obj.get("$ref") {
            let abs_ref =
                root.base_url(ref_)
                    .join(ref_)
                    .map_err(|e| CompileError::LoadUrlError {
                        url: ref_.clone(),
                        src: e.into(),
                    })?;
            s.ref_ = Some(schemas.enqueue(queue, abs_ref.into()));
            if root.draft.version < 2019 {
                // All other properties in a "$ref" object MUST be ignored
                return Ok(s);
            }
        }

        if let Some(t) = obj.get("type") {
            match t {
                Value::String(t) => s.types.extend(Type::from_str(t)),
                Value::Array(tt) => {
                    s.types.extend(tt.iter().filter_map(|t| {
                        if let Value::String(t) = t {
                            Type::from_str(t)
                        } else {
                            None
                        }
                    }));
                }
                _ => {}
            }
        }

        if let Some(Value::Array(e)) = obj.get("enum") {
            s.enum_ = e.clone();
        }

        s.minimum = load_num("minimum");
        if let Some(Value::Bool(exclusive)) = obj.get("exclusiveMinimum") {
            if *exclusive {
                (s.minimum, s.exclusive_minimum) = (None, s.minimum);
            }
        } else {
            s.exclusive_minimum = load_num("exclusiveMinimum");
        }

        s.maximum = load_num("maximum");
        if let Some(Value::Bool(exclusive)) = obj.get("exclusiveMaximum") {
            if *exclusive {
                (s.maximum, s.exclusive_maximum) = (None, s.maximum);
            }
        } else {
            s.exclusive_maximum = load_num("exclusiveMaximum");
        }

        s.multiple_of = load_num("multipleOf");

        s.min_properties = load_usize("minProperties");
        s.max_properties = load_usize("maxProperties");

        if let Some(req) = obj.get("required") {
            s.required = to_strings(req);
        }

        s.min_items = load_usize("minItems");
        s.max_items = load_usize("maxItems");
        if let Some(Value::Bool(unique)) = obj.get("uniqueItems") {
            s.unique_items = *unique;
        }

        s.min_length = load_usize("minLength");
        s.max_length = load_usize("maxLength");

        if let Some(Value::String(p)) = obj.get("pattern") {
            s.pattern = Some(Regex::new(p).map_err(|e| CompileError::Bug(e.into()))?);
        }

        s.not = load_schema("not", queue);
        s.all_of = load_schema_arr("allOf", queue);
        s.any_of = load_schema_arr("anyOf", queue);
        s.one_of = load_schema_arr("oneOf", queue);
        s.properties = load_schema_map("properties", queue);
        s.pattern_properties = {
            let mut v = vec![];
            if let Some(Value::Object(obj)) = obj.get("patternProperties") {
                for pname in obj.keys() {
                    let regex = Regex::new(pname).map_err(|e| CompileError::Bug(e.into()))?;
                    let sch = schemas
                        .enqueue(queue, format!("{loc}/patternProperties/{}", escape(pname)));
                    v.push((regex, sch));
                }
            }
            v
        };

        s.additional_properties = {
            if let Some(Value::Bool(b)) = obj.get("additionalProperties") {
                Some(Additional::Bool(*b))
            } else {
                load_schema("additionalProperties", queue).map(Additional::SchemaRef)
            }
        };

        if root.draft.version < 2020 {
            match obj.get("items") {
                Some(Value::Array(_)) => {
                    s.items = Some(Items::SchemaRefs(load_schema_arr("items", queue)));
                    s.additional_items = {
                        if let Some(Value::Bool(b)) = obj.get("additionalItems") {
                            Some(Additional::Bool(*b))
                        } else {
                            load_schema("additionalItems", queue).map(Additional::SchemaRef)
                        }
                    };
                }
                _ => s.items = load_schema("items", queue).map(Items::SchemaRef),
            }
        }

        if let Some(Value::Object(obj)) = obj.get("dependencies") {
            s.dependencies = obj
                .iter()
                .filter_map(|(k, v)| {
                    let v = match v {
                        Value::Array(_) => Some(Dependency::Props(to_strings(v))),
                        Value::Object(_) => Some(Dependency::SchemaRef(
                            schemas.enqueue(queue, format!("{loc}/dependencies/{}", escape(k))),
                        )),
                        _ => None,
                    };
                    v.map(|v| (k.clone(), v))
                })
                .collect();
        }

        // draft6 --
        if root.draft.version >= 6 {
            s.property_names = load_schema("propertyNames", queue);
            s.contains = load_schema("contains", queue);
        }

        // draft7 --
        if root.draft.version >= 7 {
            s.if_ = load_schema("if", queue);
            if s.if_.is_some() {
                s.then = load_schema("then", queue);
                s.else_ = load_schema("else", queue);
            }
        }

        // draft2019 --
        if root.draft.version >= 2019 {
            if s.contains.is_some() {
                s.min_contains = load_usize("minContains");
                s.max_contains = load_usize("maxContains");
            }
            s.dependent_schemas = load_schema_map("dependentSchemas", queue);

            if let Some(Value::Object(deps)) = obj.get("dependentRequired") {
                for (pname, pvalue) in deps {
                    s.dependent_required
                        .insert(pname.clone(), to_strings(pvalue));
                }
            }
        }

        // draft2020 --
        if root.draft.version >= 2020 {
            s.contains_marks_evaluated = true;
            s.prefix_items = load_schema_arr("prefixItems", queue);
            s.items2020 = load_schema("items", queue);
        }

        Ok(s)
    }
}

#[derive(Debug)]
pub enum CompileError {
    ParseUrlError { url: String, src: Box<dyn Error> },
    LoadUrlError { url: String, src: Box<dyn Error> },
    UnsupportedUrl { url: String },
    InvalidMetaSchema { url: String },
    MetaSchemaCycle { url: String },
    InvalidId { loc: String },
    DuplicateId { url: String, id: String },
    InvalidJsonPointer(String),
    UrlFragmentNotFound(String),
    Bug(Box<dyn Error>),
}

impl Error for CompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LoadUrlError { src, .. } => Some(src.as_ref()),
            _ => None,
        }
    }
}

impl Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseUrlError { url, src } => {
                if f.alternate() {
                    write!(f, "error parsing url {url}: {src}")
                } else {
                    write!(f, "error parsing {url}")
                }
            }
            Self::LoadUrlError { url, src } => {
                if f.alternate() {
                    write!(f, "error loading {url}: {src}")
                } else {
                    write!(f, "error loading {url}")
                }
            }
            Self::UnsupportedUrl { url } => write!(f, "loading {url} unsupported"),
            Self::InvalidMetaSchema { url } => write!(f, "invalid $schema in {url}"),
            Self::MetaSchemaCycle { url } => {
                write!(f, "cycle in resolving $schema in {url}")
            }
            Self::InvalidId { loc } => write!(f, "invalid $id at {loc}"),
            Self::DuplicateId { url, id } => write!(f, "duplicate $id {id} in {url}"),
            Self::InvalidJsonPointer(loc) => write!(f, "invalid json pointer {loc}"),
            Self::UrlFragmentNotFound(loc) => write!(f, "fragment in {loc} not found"),
            Self::Bug(src) => {
                write!(
                    f,
                    "encountered bug in jsonschema compiler. please report: {src}"
                )
            }
        }
    }
}

impl From<BorrowMutError> for CompileError {
    fn from(value: BorrowMutError) -> Self {
        Self::Bug(value.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compiler() {
        let sch: Value = serde_json::from_str(r#"{"type":"string"}"#).unwrap();
        let mut c = Compiler::default();
        let url = Url::parse("http://a.com/schema.json").unwrap();
        c.roots.or_insert(url.clone(), sch).unwrap();
        let loc = format!("{url}#");
        let mut schemas = Schemas::default();
        let sch_index = c.compile(&mut schemas, loc.clone()).unwrap();
        let inst: Value = Value::String("xx".into());
        schemas.validate(&inst, sch_index).unwrap();
    }

    #[test]
    fn test_debug() {
        run_single(
            Draft::V4,
            r#"{
                "maxLength": 2
            }"#,
            r#""foo""#,
            false,
        );
    }

    fn run_single(draft: Draft, schema: &str, data: &str, valid: bool) {
        let schema: Value = serde_json::from_str(schema).unwrap();
        let data: Value = serde_json::from_str(data).unwrap();

        let url = "http://testsuite.com/schema.json";
        let mut schemas = Schemas::default();
        let mut compiler = Compiler::default();
        compiler.set_default_draft(draft);
        compiler.add_resource(url, schema).unwrap();
        let sch_index = compiler.compile(&mut schemas, url.into()).unwrap();
        let result = schemas.validate(&data, sch_index);
        if let Err(e) = &result {
            println!("{e:#}");
        }
        assert_eq!(result.is_ok(), valid);
    }
}
