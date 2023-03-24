use std::{borrow::Cow, cmp::min, collections::HashSet, fmt::Write};

use serde_json::{Map, Value};

use crate::{util::*, *};

macro_rules! kw {
    ($kw:expr) => {
        Some(KeywordPath {
            keyword: $kw,
            token: None,
        })
    };
}

macro_rules! kw_prop {
    ($kw:expr, $prop:expr) => {
        Some(KeywordPath {
            keyword: $kw,
            token: Some(SchemaToken::Prop($prop)),
        })
    };
}

#[allow(unused)]
macro_rules! kw_item {
    ($kw:expr, $item:expr) => {
        Some(KeywordPath {
            keyword: $kw,
            token: Some(SchemaToken::Item($item)),
        })
    };
}

pub(crate) fn validate<'s, 'v>(
    v: &'v Value,
    schema: &'s Schema,
    schemas: &'s Schemas,
) -> Result<(), ValidationError<'s, 'v>> {
    let scope = Scope {
        sch: schema.idx,
        ref_kw: None,
        vid: 0,
        parent: None,
    };
    let mut vloc = Vec::with_capacity(8);
    let result = Validator {
        v,
        schema,
        schemas,
        scope,
        uneval: Uneval::from(v, schema, false),
        errors: vec![],
        bool_result: false,
    }
    .validate(&mut JsonPointer::new(&mut vloc));
    match result {
        Err(err) => {
            let mut e = ValidationError {
                absolute_keyword_location: AbsoluteKeywordLocation::new(schema),
                instance_location: InstanceLocation::new(),
                kind: ErrorKind::Schema { url: &schema.loc },
                causes: vec![],
            };
            if let ErrorKind::Group = err.kind {
                e.causes = err.causes;
            } else {
                e.causes.push(err);
            }
            Err(e)
        }
        Ok(_) => Ok(()),
    }
}

macro_rules! kind {
    ($kind:ident, $name:ident: $value:expr) => {
        ErrorKind::$kind { $name: $value }
    };
    ($kind:ident, $got:expr, $want:expr) => {
        ErrorKind::$kind {
            got: $got,
            want: $want,
        }
    };
    ($kind:ident, $got:expr, $want:expr, $err:expr) => {
        ErrorKind::$kind {
            got: $got,
            want: $want,
            err: $err,
        }
    };
    ($kind: ident) => {
        ErrorKind::$kind
    };
}

struct Validator<'v, 's, 'd> {
    v: &'v Value,
    schema: &'s Schema,
    schemas: &'s Schemas,
    scope: Scope<'d>,
    uneval: Uneval<'v>,
    errors: Vec<ValidationError<'s, 'v>>,
    bool_result: bool,
}

impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn validate(
        mut self,
        vloc: &mut JsonPointer<'_, 'v>,
    ) -> Result<Uneval<'v>, ValidationError<'s, 'v>> {
        let s = self.schema;
        let v = self.v;

        // boolean --
        if let Some(b) = s.boolean {
            return match b {
                false => Err(self.error(None, vloc, kind!(FalseSchema))),
                true => Ok(self.uneval),
            };
        }

        if let Some(scp) = self.scope.check_cycle() {
            let kind = ErrorKind::RefCycle {
                url: &self.schema.loc,
                kw_loc1: self.kw_loc(&self.scope),
                kw_loc2: self.kw_loc(scp),
            };
            return Err(self.error(None, vloc, kind));
        }

        // type --
        if !s.types.is_empty() {
            let v_type = Type::of(v);
            let matched =
                s.types.contains(v_type) || (s.types.contains(Type::Integer) && is_integer(v));
            if !matched {
                return Err(self.error(kw!("type"), vloc, kind!(Type, v_type, s.types)));
            }
        }

        // enum --
        if let Some(Enum { types, values }) = &s.enum_ {
            if !types.contains(Type::of(v)) || !values.iter().any(|e| equals(e, v)) {
                let kind = kind!(Enum, v.clone(), values);
                return Err(self.error(kw!("enum"), vloc, kind));
            }
        }

        // constant --
        if let Some(c) = &s.constant {
            if !equals(v, c) {
                return Err(self.error(kw!("const"), vloc, kind!(Const, v.clone(), c)));
            }
        }

        // $ref --
        if let Some(ref_) = s.ref_ {
            let result = self.validate_ref(ref_, "$ref", vloc);
            if s.draft_version < 2019 {
                return result.map(|_| self.uneval);
            }
            self.errors.extend(result.err());
        }

        // format --
        if let Some(format) = &s.format {
            if let Err(e) = (format.func)(v) {
                let kind = kind!(Format, v.clone(), format.name, e);
                self.add_error(kw!("format"), vloc, kind);
            }
        }

        match v {
            Value::Object(obj) => self.obj_validate(obj, vloc),
            Value::Array(arr) => self.arr_validate(arr, vloc),
            Value::String(str) => self.str_validate(str, vloc),
            Value::Number(num) => self.num_validate(num, vloc),
            _ => {}
        }

        if !self.bool_result || self.errors.is_empty() {
            if s.draft_version >= 2019 {
                self.refs_validate(vloc);
            }
            self.cond_validate(vloc);
            if s.draft_version >= 2019 && self.errors.is_empty() {
                self.uneval_validate(vloc);
            }
        }

        match self.errors.len() {
            0 => Ok(self.uneval),
            1 => Err(self.errors.remove(0)),
            _ => {
                let mut e = self.error(None, vloc, kind!(Group));
                e.causes = self.errors;
                Err(e)
            }
        }
    }
}

// type specific validations
impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn obj_validate(&mut self, obj: &'v Map<String, Value>, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;
        macro_rules! add_err {
            ($result:expr) => {
                let result = $result;
                self.errors.extend(result.err());
            };
        }

        // minProperties --
        if let Some(min) = s.min_properties {
            if obj.len() < min {
                let kind = kind!(MinProperties, obj.len(), min);
                self.add_error(kw!("minProperties"), vloc, kind);
            }
        }

        // maxProperties --
        if let Some(max) = s.max_properties {
            if obj.len() > max {
                let kind = kind!(MaxProperties, obj.len(), max);
                self.add_error(kw!("maxProperties"), vloc, kind);
            }
        }

        // propertyNames --
        if let Some(sch) = &s.property_names {
            for pname in obj.keys() {
                //todo: use pname as value(tip: use enum{PropName|Value})
                let v = Value::String(pname.to_owned());
                let mut vec = Vec::with_capacity(vloc.len);
                let mut vloc = vloc.clone_static(&mut vec);
                if let Err(e) = self.validate_val(*sch, &v, &mut vloc) {
                    self.errors.push(e.clone_static());
                }
            }
        }

        // required --
        if let Some(missing) = self.find_missing(obj, &s.required) {
            self.add_error(kw!("required"), vloc, kind!(Required, want: missing));
        }

        if self.bool_result && !self.errors.is_empty() {
            return;
        }

        // dependencies --
        for (prop, dependency) in &s.dependencies {
            if obj.contains_key(prop) {
                match dependency {
                    Dependency::Props(required) => {
                        if let Some(missing) = self.find_missing(obj, required) {
                            let kind = ErrorKind::Dependency { prop, missing };
                            self.add_error(kw_prop!("dependencies", prop), vloc, kind);
                        }
                    }
                    Dependency::SchemaRef(sch) => {
                        if let Err(e) = self.validate_self(*sch, vloc) {
                            self.errors.push(e);
                        }
                    }
                }
            }
        }

        // dependentSchemas --
        for (pname, sch) in &s.dependent_schemas {
            if obj.contains_key(pname) {
                add_err!(self.validate_self(*sch, vloc));
            }
        }

        // dependentRequired --
        for (prop, required) in &s.dependent_required {
            if obj.contains_key(prop) {
                if let Some(missing) = self.find_missing(obj, required) {
                    let kind = ErrorKind::DependentRequired { prop, missing };
                    self.add_error(kw_prop!("dependentRequired", prop), vloc, kind);
                }
            }
        }

        for (pname, pvalue) in obj {
            if self.bool_result && !self.errors.is_empty() {
                return;
            }
            let mut evaluated = false;

            // properties --
            if let Some(sch) = s.properties.get(pname) {
                match self.validate_val(*sch, pvalue, &mut vloc.prop(pname)) {
                    Ok(_) => evaluated = true,
                    Err(e) => self.errors.push(e),
                }
            }

            // patternProperties --
            for (regex, sch) in &s.pattern_properties {
                if regex.is_match(pname) {
                    match self.validate_val(*sch, pvalue, &mut vloc.prop(pname)) {
                        Ok(_) => evaluated = true,
                        Err(e) => self.errors.push(e),
                    }
                }
            }

            if !evaluated {
                // additionalProperties --
                if let Some(additional) = &s.additional_properties {
                    match additional {
                        Additional::Bool(allowed) => {
                            if !allowed {
                                let kind = kind!(AdditionalProperty, got: pname.clone());
                                self.add_error(kw!("additionalProperties"), vloc, kind);
                            }
                        }
                        Additional::SchemaRef(sch) => {
                            add_err!(self.validate_val(*sch, pvalue, &mut vloc.prop(pname)));
                        }
                    }
                    evaluated = true;
                }
            }

            if evaluated {
                self.uneval.props.remove(pname);
            }
        }
    }

    fn arr_validate(&mut self, arr: &'v Vec<Value>, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;
        macro_rules! add_err {
            ($result:expr) => {
                let result = $result;
                self.errors.extend(result.err());
            };
        }

        // minItems --
        if let Some(min) = s.min_items {
            if arr.len() < min {
                self.add_error(kw!("minItems"), vloc, kind!(MinItems, arr.len(), min));
            }
        }

        // maxItems --
        if let Some(max) = s.max_items {
            if arr.len() > max {
                self.add_error(kw!("maxItems"), vloc, kind!(MaxItems, arr.len(), max));
            }
        }

        // uniqueItems --
        if s.unique_items {
            'outer: for i in 1..arr.len() {
                for j in 0..i {
                    if equals(&arr[i], &arr[j]) {
                        let kind = kind!(UniqueItems, got: [j, i]);
                        self.add_error(kw!("uniqueItems"), vloc, kind);
                        break 'outer;
                    }
                }
            }
        }

        if s.draft_version < 2020 {
            let mut evaluated = 0;

            // items --
            if let Some(items) = &s.items {
                match items {
                    Items::SchemaRef(sch) => {
                        for (i, item) in arr.iter().enumerate() {
                            add_err!(self.validate_val(*sch, item, &mut vloc.item(i)));
                        }
                        evaluated = arr.len();
                        debug_assert!(self.uneval.items.is_empty());
                    }
                    Items::SchemaRefs(list) => {
                        for (i, (item, sch)) in arr.iter().zip(list).enumerate() {
                            self.uneval.items.remove(&i);
                            add_err!(self.validate_val(*sch, item, &mut vloc.item(i)));
                        }
                        evaluated = min(list.len(), arr.len());
                    }
                }
            }

            // additionalItems --
            if let Some(additional) = &s.additional_items {
                match additional {
                    Additional::Bool(allowed) => {
                        if !allowed && evaluated != arr.len() {
                            let kind = kind!(AdditionalItems, got: arr.len() - evaluated);
                            self.add_error(kw!("additionalItems"), vloc, kind);
                        }
                    }
                    Additional::SchemaRef(sch) => {
                        for (i, item) in arr[evaluated..].iter().enumerate() {
                            add_err!(self.validate_val(*sch, item, &mut vloc.item(i)));
                        }
                    }
                }
                debug_assert!(self.uneval.items.is_empty());
            }
        } else {
            // prefixItems --
            for (i, (sch, item)) in s.prefix_items.iter().zip(arr).enumerate() {
                self.uneval.items.remove(&i);
                add_err!(self.validate_val(*sch, item, &mut vloc.item(i)));
            }

            // items2020 --
            if let Some(sch) = &s.items2020 {
                let evaluated = min(s.prefix_items.len(), arr.len());
                for (i, item) in arr[evaluated..].iter().enumerate() {
                    add_err!(self.validate_val(*sch, item, &mut vloc.item(i)));
                }
                debug_assert!(self.uneval.items.is_empty());
            }
        }

        // contains --
        let mut contains_matched = vec![];
        let mut contains_errors = vec![];
        if let Some(sch) = &s.contains {
            for (i, item) in arr.iter().enumerate() {
                if let Err(e) = self.validate_val(*sch, item, &mut vloc.item(i)) {
                    contains_errors.push(e);
                } else {
                    contains_matched.push(i);
                    if s.draft_version >= 2020 {
                        self.uneval.items.remove(&i);
                    }
                }
            }
        }

        // minContains --
        if let Some(min) = s.min_contains {
            if contains_matched.len() < min {
                let kind = kind!(MinContains, contains_matched.clone(), min);
                let mut e = self.error(kw!("minContains"), vloc, kind);
                e.causes = contains_errors;
                self.errors.push(e);
            }
        } else if s.contains.is_some() && contains_matched.is_empty() {
            let mut e = self.error(kw!("contains"), vloc, kind!(Contains));
            e.causes = contains_errors;
            self.errors.push(e);
        }

        // maxContains --
        if let Some(max) = s.max_contains {
            if contains_matched.len() > max {
                let kind = kind!(MaxContains, contains_matched, max);
                self.add_error(kw!("maxContains"), vloc, kind);
            }
        }
    }

    fn str_validate(&mut self, str: &'v String, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;
        let mut len = None;

        // minLength --
        if let Some(min) = s.min_length {
            let len = len.get_or_insert_with(|| str.chars().count());
            if *len < min {
                self.add_error(kw!("minLength"), vloc, kind!(MinLength, *len, min));
            }
        }

        // maxLength --
        if let Some(max) = s.max_length {
            let len = len.get_or_insert_with(|| str.chars().count());
            if *len > max {
                self.add_error(kw!("maxLength"), vloc, kind!(MaxLength, *len, max));
            }
        }

        // pattern --
        if let Some(regex) = &s.pattern {
            if !regex.is_match(str) {
                let kind = kind!(Pattern, str.clone(), regex.as_str());
                self.add_error(kw!("pattern"), vloc, kind);
            }
        }

        if s.draft_version >= 7 {
            // contentEncoding --
            let mut decoded = Cow::from(str.as_bytes());
            if let Some(decoder) = &s.content_encoding {
                match (decoder.func)(str) {
                    Ok(bytes) => decoded = Cow::from(bytes),
                    Err(e) => {
                        let kind = kind!(ContentEncoding, str.clone(), decoder.name, e);
                        self.add_error(kw!("contentEncoding"), vloc, kind)
                    }
                }
            }

            // contentMediaType --
            let mut deserialized = None;
            if let Some(mt) = &s.content_media_type {
                match (mt.func)(decoded.as_ref(), s.content_schema.is_some()) {
                    Ok(des) => deserialized = des,
                    Err(e) => {
                        let kind = kind!(ContentMediaType, decoded.into(), mt.name, e);
                        self.add_error(kw!("contentMediaType"), vloc, kind);
                    }
                }
            }

            // contentSchema --
            if let (Some(sch), Some(v)) = (s.content_schema, deserialized) {
                // todo: check if keywordLocation is correct
                if let Err(mut e) = self.schemas.validate(&v, sch) {
                    e.kind = kind!(ContentSchema);
                    self.errors.push(e.clone_static());
                }
            }
        }
    }

    fn num_validate(&mut self, num: &'v Number, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;

        // minimum --
        if let Some(min) = &s.minimum {
            if let (Some(minf), Some(numf)) = (min.as_f64(), num.as_f64()) {
                if numf < minf {
                    let kind = kind!(Minimum, num.clone(), min.clone());
                    self.add_error(kw!("minimum"), vloc, kind);
                }
            }
        }

        // maximum --
        if let Some(max) = &s.maximum {
            if let (Some(maxf), Some(numf)) = (max.as_f64(), num.as_f64()) {
                if numf > maxf {
                    let kind = kind!(Maximum, num.clone(), max.clone());
                    self.add_error(kw!("maximum"), vloc, kind);
                }
            }
        }

        // exclusiveMinimum --
        if let Some(ex_min) = &s.exclusive_minimum {
            if let (Some(ex_minf), Some(numf)) = (ex_min.as_f64(), num.as_f64()) {
                if numf <= ex_minf {
                    let kind = kind!(ExclusiveMinimum, num.clone(), ex_min.clone());
                    self.add_error(kw!("exclusiveMinimum"), vloc, kind);
                }
            }
        }

        // exclusiveMaximum --
        if let Some(ex_max) = &s.exclusive_maximum {
            if let (Some(ex_maxf), Some(numf)) = (ex_max.as_f64(), num.as_f64()) {
                if numf >= ex_maxf {
                    let kind = kind!(ExclusiveMaximum, num.clone(), ex_max.clone());
                    self.add_error(kw!("exclusiveMaximum"), vloc, kind);
                }
            }
        }

        // multipleOf --
        if let Some(mul) = &s.multiple_of {
            if let (Some(mulf), Some(numf)) = (mul.as_f64(), num.as_f64()) {
                if (numf / mulf).fract() != 0.0 {
                    let kind = kind!(MultipleOf, num.clone(), mul.clone());
                    self.add_error(kw!("multipleOf"), vloc, kind);
                }
            }
        }
    }
}

// references validation
impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn refs_validate(&mut self, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;
        macro_rules! add_err {
            ($result:expr) => {
                let result = $result;
                self.errors.extend(result.err());
            };
        }

        // $recursiveRef --
        if let Some(mut sch) = s.recursive_ref {
            if self.schemas.get(sch).recursive_anchor {
                sch = self.resolve_recursive_anchor().unwrap_or(sch);
            }
            add_err!(self.validate_ref(sch, "$recursiveRef", vloc));
        }

        // $dynamicRef --
        if let Some(dref) = &s.dynamic_ref {
            let mut sch = dref.sch; // initial target
            if let Some(anchor) = &dref.anchor {
                // $dynamicRef includes anchor
                if self.schemas.get(sch).dynamic_anchor == dref.anchor {
                    // initial target has matching $dynamicAnchor
                    sch = self.resolve_dynamic_anchor(anchor).unwrap_or(sch);
                }
            }
            add_err!(self.validate_ref(sch, "$dynamicRef", vloc));
        }
    }

    fn validate_ref(
        &mut self,
        sch: SchemaIndex,
        kw: &'static str,
        vloc: &mut JsonPointer<'_, 'v>,
    ) -> Result<(), ValidationError<'s, 'v>> {
        if let Err(err) = self._validate_self(sch, kw.into(), vloc, false) {
            let url = &self.schemas.get(sch).loc;
            let mut ref_err = self.error(kw!(kw), vloc, ErrorKind::Reference { url });
            if let ErrorKind::Group = err.kind {
                ref_err.causes = err.causes;
            } else {
                ref_err.causes.push(err);
            }
            return Err(ref_err);
        }
        Ok(())
    }

    fn resolve_recursive_anchor(&self) -> Option<SchemaIndex> {
        let mut scope = &self.scope;
        let mut sch = None;
        loop {
            let scope_sch = self.schemas.get(scope.sch);
            let base_sch = self.schemas.get(scope_sch.resource);
            if base_sch.recursive_anchor {
                sch.replace(scope.sch);
            }
            if let Some(parent) = scope.parent {
                scope = parent;
            } else {
                return sch;
            }
        }
    }

    fn resolve_dynamic_anchor(&self, name: &String) -> Option<SchemaIndex> {
        let mut scope = &self.scope;
        let mut sch = None;
        loop {
            let scope_sch = self.schemas.get(scope.sch);
            let base_sch = self.schemas.get(scope_sch.resource);
            debug_assert_eq!(base_sch.idx, base_sch.resource);
            if let Some(dsch) = base_sch.dynamic_anchors.get(name) {
                sch.replace(*dsch);
            }
            if let Some(parent) = scope.parent {
                scope = parent;
            } else {
                return sch;
            }
        }
    }
}

// conditional validation
impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn cond_validate(&mut self, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;
        macro_rules! add_err {
            ($result:expr) => {
                let result = $result;
                self.errors.extend(result.err());
            };
        }

        // not --
        if let Some(not) = s.not {
            if self._validate_self(not, None, vloc, true).is_ok() {
                self.add_error(kw!("not"), vloc, kind!(Not));
            }
        }

        // allOf --
        if !s.all_of.is_empty() {
            let mut allof_errors = vec![];
            for sch in &s.all_of {
                if let Err(e) = self.validate_self(*sch, vloc) {
                    allof_errors.push(e);
                    if self.bool_result {
                        break;
                    }
                }
            }
            if !allof_errors.is_empty() {
                self.add_errors(allof_errors, kw!("allOf"), vloc, kind!(AllOf));
            }
        }

        // anyOf --
        if !s.any_of.is_empty() {
            let mut matched = false;
            let mut anyof_errors = vec![];
            for sch in &s.any_of {
                match self.validate_self(*sch, vloc) {
                    Ok(_) => {
                        matched = true;
                        // for uneval, all schemas must be checked
                        if self.uneval.is_empty() {
                            break;
                        }
                    }
                    Err(e) => anyof_errors.push(e),
                }
            }
            if !matched {
                self.add_errors(anyof_errors, kw!("anyOf"), vloc, kind!(AnyOf));
            }
        }

        // oneOf --
        if !s.one_of.is_empty() {
            let (mut matched, mut oneof_errors) = (None, vec![]);
            for (i, sch) in s.one_of.iter().enumerate() {
                if let Err(e) = self._validate_self(*sch, None, vloc, matched.is_some()) {
                    if matched.is_none() {
                        oneof_errors.push(e);
                    }
                } else {
                    match matched {
                        None => _ = matched.replace(i),
                        Some(prev) => {
                            let kind = ErrorKind::OneOf(Some((prev, i)));
                            self.add_error(kw!("oneOf"), vloc, kind);
                        }
                    }
                }
            }
            if matched.is_none() {
                let kind = ErrorKind::OneOf(None);
                self.add_errors(oneof_errors, kw!("oneOf"), vloc, kind);
            }
        }

        // if, then, else --
        if let Some(if_) = s.if_ {
            if self._validate_self(if_, None, vloc, true).is_ok() {
                if let Some(then) = s.then {
                    add_err!(self.validate_self(then, vloc));
                }
            } else if let Some(else_) = s.else_ {
                add_err!(self.validate_self(else_, vloc));
            }
        }
    }
}

// uneval validation
impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn uneval_validate(&mut self, vloc: &mut JsonPointer<'_, 'v>) {
        let s = self.schema;
        let v = self.v;
        macro_rules! add_err {
            ($result:expr) => {
                let result = $result;
                self.errors.extend(result.err());
            };
        }

        // unevaluatedProps --
        if let (Some(sch), Value::Object(obj)) = (s.unevaluated_properties, v) {
            for pname in &self.uneval.props {
                if let Some(pvalue) = obj.get(*pname) {
                    add_err!(self.validate_val(sch, pvalue, &mut vloc.prop(pname)));
                }
            }
            self.uneval.props.clear();
        }

        // unevaluatedItems --
        if let (Some(sch), Value::Array(arr)) = (s.unevaluated_items, v) {
            for i in &self.uneval.items {
                if let Some(pvalue) = arr.get(*i) {
                    add_err!(self.validate_val(sch, pvalue, &mut vloc.item(*i)));
                }
            }
            self.uneval.items.clear();
        }
    }
}

// validation helpers
impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn validate_val(
        &self,
        sch: SchemaIndex,
        v: &'v Value,
        vloc: &mut JsonPointer<'_, 'v>,
    ) -> Result<(), ValidationError<'s, 'v>> {
        let scope = Scope::child(sch, None, self.scope.vid + 1, &self.scope);
        let schema = &self.schemas.get(sch);
        Validator {
            v,
            schema,
            schemas: self.schemas,
            scope,
            uneval: Uneval::from(v, schema, false),
            errors: vec![],
            bool_result: self.bool_result,
        }
        .validate(vloc)
        .map(|_| ())
    }

    fn _validate_self(
        &mut self,
        sch: SchemaIndex,
        ref_kw: Option<&'static str>,
        vloc: &mut JsonPointer<'_, 'v>,
        bool_result: bool,
    ) -> Result<(), ValidationError<'s, 'v>> {
        let scope = Scope::child(sch, ref_kw, self.scope.vid, &self.scope);
        let schema = &self.schemas.get(sch);
        let result = Validator {
            v: self.v,
            schema,
            schemas: self.schemas,
            scope,
            uneval: Uneval::from(self.v, schema, !self.uneval.is_empty()),
            errors: vec![],
            bool_result: self.bool_result || bool_result,
        }
        .validate(vloc);
        if let Ok(reply) = &result {
            self.uneval.merge(reply);
        }
        result.map(|_| ())
    }

    #[inline(always)]
    fn validate_self(
        &mut self,
        sch: SchemaIndex,
        vloc: &mut JsonPointer<'_, 'v>,
    ) -> Result<(), ValidationError<'s, 'v>> {
        self._validate_self(sch, None, vloc, false)
    }
}

// error helpers
impl<'v, 's, 'd> Validator<'v, 's, 'd> {
    fn error(
        &self,
        kw_path: Option<KeywordPath<'s>>,
        vloc: &JsonPointer<'_, 'v>,
        kind: ErrorKind<'s>,
    ) -> ValidationError<'s, 'v> {
        if self.bool_result {
            return ValidationError {
                absolute_keyword_location: AbsoluteKeywordLocation {
                    schema_url: "",
                    keyword_path: None,
                },
                instance_location: InstanceLocation::new(),
                kind: ErrorKind::Group,
                causes: vec![],
            };
        }
        ValidationError {
            absolute_keyword_location: AbsoluteKeywordLocation {
                schema_url: &self.schema.loc,
                keyword_path: kw_path,
            },
            instance_location: vloc.into(),
            kind,
            causes: vec![],
        }
    }

    fn add_error(
        &mut self,
        kw_path: Option<KeywordPath<'s>>,
        vloc: &JsonPointer<'_, 'v>,
        kind: ErrorKind<'s>,
    ) {
        self.errors.push(self.error(kw_path, vloc, kind));
    }

    fn add_errors(
        &mut self,
        errors: Vec<ValidationError<'s, 'v>>,
        kw_path: Option<KeywordPath<'s>>,
        vloc: &JsonPointer<'_, 'v>,
        kind: ErrorKind<'s>,
    ) {
        if errors.len() == 1 {
            self.errors.extend(errors);
        } else {
            let mut err = self.error(kw_path, vloc, kind);
            err.causes = errors;
            self.errors.push(err);
        }
    }

    fn kw_loc(&self, mut scope: &Scope) -> String {
        let mut loc = String::new();
        while let Some(parent) = scope.parent {
            let kw_path = scope.ref_kw.unwrap_or_else(|| {
                let cur = &self.schemas.get(scope.sch).loc;
                let parent = &self.schemas.get(parent.sch).loc;
                &cur[parent.len()..]
            });
            loc.insert_str(0, kw_path);
            scope = parent;
        }
        loc
    }

    fn find_missing(
        &self,
        obj: &'v Map<String, Value>,
        required: &'s [String],
    ) -> Option<Vec<&'s str>> {
        let mut missing = required
            .iter()
            .filter(|p| !obj.contains_key(p.as_str()))
            .map(|p| p.as_str());
        if self.bool_result {
            missing.next().map(|_| Vec::new())
        } else {
            let missing = missing.collect::<Vec<_>>();
            if missing.is_empty() {
                None
            } else {
                Some(missing)
            }
        }
    }
}

// Uneval --

#[derive(Default)]
struct Uneval<'v> {
    props: HashSet<&'v String>,
    items: HashSet<usize>,
}

impl<'v> Uneval<'v> {
    fn is_empty(&self) -> bool {
        self.props.is_empty() && self.items.is_empty()
    }

    fn from(v: &'v Value, sch: &Schema, caller_needs: bool) -> Self {
        let mut uneval = Self::default();
        match v {
            Value::Object(obj) => {
                if !sch.all_props_evaluated
                    && (caller_needs || sch.unevaluated_properties.is_some())
                {
                    uneval.props = obj.keys().collect();
                }
            }
            Value::Array(arr) => {
                if !sch.all_items_evaluated && (caller_needs || sch.unevaluated_items.is_some()) {
                    uneval.items = (0..arr.len()).collect();
                }
            }
            _ => (),
        }
        uneval
    }

    fn merge(&mut self, other: &Uneval) {
        self.props.retain(|p| other.props.contains(p));
        self.items.retain(|i| other.items.contains(i));
    }
}

// Scope ---

#[derive(Debug)]
struct Scope<'a> {
    sch: SchemaIndex,
    // if None, compute from self.sch and self.parent.sh
    // not None only when there is jump i.e $ref, $XXXRef
    ref_kw: Option<&'static str>,
    /// unique id of value being validated
    // if two scope validate same value, they will have same vid
    vid: usize,
    parent: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    fn child(
        sch: SchemaIndex,
        ref_kw: Option<&'static str>,
        vid: usize,
        parent: &'a Scope,
    ) -> Self {
        Self {
            sch,
            ref_kw,
            vid,
            parent: Some(parent),
        }
    }

    fn check_cycle(&self) -> Option<&Scope> {
        let mut scope = self.parent;
        while let Some(scp) = scope {
            if scp.vid != self.vid {
                break;
            }
            if scp.sch == self.sch {
                return Some(scp);
            }
            scope = scp.parent;
        }
        None
    }
}

/// Token in InstanceLocation json-pointer.
#[derive(Debug, Clone)]
pub enum InstanceToken<'v> {
    /// Token for property.
    Prop(Cow<'v, str>),
    /// Token for array item.
    Item(usize),
}

impl<'v> InstanceToken<'v> {
    fn to_string(tokens: &[InstanceToken]) -> String {
        use InstanceToken::*;
        let mut r = String::new();
        for tok in tokens {
            r.push('/');
            match tok {
                Prop(s) => r.push_str(&escape(s)),
                Item(i) => write!(&mut r, "{i}").expect("write to String should never fail"),
            }
        }
        r
    }
}

impl<'v> From<String> for InstanceToken<'v> {
    fn from(prop: String) -> Self {
        InstanceToken::Prop(prop.into())
    }
}

impl<'v> From<&'v str> for InstanceToken<'v> {
    fn from(prop: &'v str) -> Self {
        InstanceToken::Prop(prop.into())
    }
}

impl<'v> From<usize> for InstanceToken<'v> {
    fn from(index: usize) -> Self {
        InstanceToken::Item(index)
    }
}

struct JsonPointer<'a, 'v> {
    vec: &'a mut Vec<InstanceToken<'v>>,
    len: usize,
}

impl<'a, 'v> JsonPointer<'a, 'v> {
    fn new(vec: &'a mut Vec<InstanceToken<'v>>) -> Self {
        let len = vec.len();
        Self { vec, len }
    }

    fn prop<'x>(&'x mut self, name: &'v str) -> JsonPointer<'x, 'v> {
        self.vec.truncate(self.len);
        self.vec.push(name.into());
        JsonPointer::new(self.vec)
    }

    fn item<'x>(&'x mut self, i: usize) -> JsonPointer<'x, 'v> {
        self.vec.truncate(self.len);
        self.vec.push(i.into());
        JsonPointer::new(self.vec)
    }

    fn clone_static<'aa, 'vv>(
        &self,
        vec: &'aa mut Vec<InstanceToken<'vv>>,
    ) -> JsonPointer<'aa, 'vv> {
        for tok in self.vec[..self.len].iter() {
            match tok {
                InstanceToken::Prop(p) => vec.push(p.as_ref().to_owned().into()),
                InstanceToken::Item(i) => vec.push((*i).into()),
            }
        }
        JsonPointer::new(vec)
    }
}

impl<'a, 'v> ToString for JsonPointer<'a, 'v> {
    fn to_string(&self) -> String {
        InstanceToken::to_string(&self.vec[..self.len])
    }
}

/// The location of the JSON value within the instance being validated
#[derive(Debug, Default)]
pub struct InstanceLocation<'v> {
    pub tokens: Vec<InstanceToken<'v>>,
}

impl<'v> InstanceLocation<'v> {
    fn new() -> Self {
        Self::default()
    }

    fn clone_static(self) -> InstanceLocation<'static> {
        let mut tokens = Vec::with_capacity(self.tokens.len());
        for tok in self.tokens {
            let tok = match tok {
                InstanceToken::Prop(p) => InstanceToken::Prop(p.into_owned().into()),
                InstanceToken::Item(i) => InstanceToken::Item(i),
            };
            tokens.push(tok);
        }
        InstanceLocation { tokens }
    }
}

impl<'a, 'v> From<&JsonPointer<'a, 'v>> for InstanceLocation<'v> {
    fn from(value: &JsonPointer<'a, 'v>) -> Self {
        let mut tokens = Vec::with_capacity(value.len);
        for tok in &value.vec[..value.len] {
            tokens.push(tok.clone());
        }
        Self { tokens }
    }
}

impl<'v> ToString for InstanceLocation<'v> {
    fn to_string(&self) -> String {
        InstanceToken::to_string(&self.tokens)
    }
}

impl<'s, 'v> ValidationError<'s, 'v> {
    pub(crate) fn clone_static(self) -> ValidationError<'s, 'static> {
        let mut causes = Vec::with_capacity(self.causes.len());
        for cause in self.causes {
            causes.push(cause.clone_static());
        }
        ValidationError {
            instance_location: self.instance_location.clone_static(),
            causes,
            ..self
        }
    }
}

// SchemaPointer --

/// Token for schema.
#[derive(Debug, Clone)]
pub enum SchemaToken<'s> {
    /// Token for property.
    Prop(&'s str),
    /// Token for array item.
    Item(usize),
}

impl<'s> Display for SchemaToken<'s> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaToken::Prop(p) => write!(f, "{}", escape(p)),
            SchemaToken::Item(i) => write!(f, "{i}"),
        }
    }
}

#[derive(Debug, Clone)]
/// JsonPointer in schema.
pub struct KeywordPath<'s> {
    /// The first token.
    pub keyword: &'static str,
    /// Optinal token within keyword.
    pub token: Option<SchemaToken<'s>>,
}

impl<'s> Display for KeywordPath<'s> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.keyword.fmt(f)?;
        if let Some(token) = &self.token {
            f.write_str("/")?;
            token.fmt(f)?;
        }
        Ok(())
    }
}

/// The absolute, dereferenced location of the validating keyword
#[derive(Debug, Clone)]
pub struct AbsoluteKeywordLocation<'s> {
    /// The absolute, dereferenced schema location.
    pub schema_url: &'s str,
    /// Location within the `schema_url`.
    pub keyword_path: Option<KeywordPath<'s>>,
}

impl<'s> AbsoluteKeywordLocation<'s> {
    fn new(s: &'s Schema) -> Self {
        Self {
            schema_url: &s.loc,
            keyword_path: None,
        }
    }
}

impl<'s> Display for AbsoluteKeywordLocation<'s> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.schema_url.fmt(f)?;
        if let Some(path) = &self.keyword_path {
            f.write_str("/")?;
            path.keyword.fmt(f)?;
            if let Some(token) = &path.token {
                f.write_str("/")?;
                match token {
                    SchemaToken::Prop(p) => write!(f, "{}", escape(p))?, // todo: url-encode
                    SchemaToken::Item(i) => write!(f, "{i}")?,
                }
            }
        }
        Ok(())
    }
}
