use std::any::Any;
use std::sync::Arc;
use std::io::Write;
use std::collections::VecDeque;

use template::Template;
use utils::is_true;
use node::*;

use gtmpl_value::{Func, Value};

struct Variable {
    name: String,
    value: Arc<Any>,
}

struct State<'a, 'b, T: Write>
where
    T: 'b,
{
    template: &'a Template<'a>,
    writer: &'b mut T,
    node: Option<&'a Nodes>,
    vars: VecDeque<VecDeque<Variable>>,
    depth: usize,
}

/// A Context for the template. Passed to the template exectution.
pub struct Context {
    dot: Arc<Any>,
}

#[derive(Clone, Debug)]
struct Nothing {}

impl Context {
    pub fn empty() -> Context {
        Context {
            dot: Arc::new(Nothing {}),
        }
    }

    pub fn from<T>(value: T) -> Result<Context, String>
    where
        T: Into<Value>,
    {
        let serialized = Value::from(value);
        Ok(Context {
            dot: Arc::new(serialized),
        })
    }

    pub fn from_any(value: Arc<Any>) -> Context {
        Context { dot: value }
    }
}

macro_rules! print_val {
    ($val:ident : $out:ident <- $($typ:ty,)*) => {
        $(
            if let Some(v) = $val.downcast_ref::<$typ>() {
                write!($out.writer, "{}", v).map_err(|e| format!("{}", e))?;
                return Ok(())
            }
        )*
    }
}

impl<'a, 'b> Template<'a> {
    pub fn execute<T: Write>(&self, writer: &'b mut T, data: &Context) -> Result<(), String> {
        let mut vars: VecDeque<VecDeque<Variable>> = VecDeque::new();
        let mut dot = VecDeque::new();
        dot.push_back(Variable {
            name: "$".to_owned(),
            value: Arc::clone(&data.dot),
        });
        vars.push_back(dot);

        let mut state = State {
            template: self,
            writer,
            node: None,
            vars,
            depth: 0,
        };

        let root = self.tree_ids
            .get(&1usize)
            .and_then(|name| self.tree_set.get(name))
            .and_then(|tree| tree.root.as_ref())
            .ok_or_else(|| format!("{} is an incomplete or empty template", self.name))?;
        state.walk(data, root)?;

        Ok(())
    }

    pub fn render(&self, data: &Context) -> Result<String, String> {
        let mut w: Vec<u8> = vec![];
        self.execute(&mut w, data)?;
        String::from_utf8(w).map_err(|e| format!("unable to contert output into utf8: {}", e))
    }
}

impl<'a, 'b, T: Write> State<'a, 'b, T> {
    fn set_kth_last_var_value(&mut self, k: usize, value: Arc<Any>) -> Result<(), String> {
        if let Some(last_vars) = self.vars.back_mut() {
            let i = last_vars.len() - k;
            if let Some(kth_last_var) = last_vars.get_mut(i) {
                kth_last_var.value = value;
                return Ok(());
            }
            return Err(format!("current var context smaller than {}", k));
        }
        Err(String::from("empty var stack"))
    }

    fn var_value(&self, key: &str) -> Result<Arc<Any>, String> {
        for context in self.vars.iter().rev() {
            for var in context.iter().rev() {
                if var.name == key {
                    return Ok(Arc::clone(&var.value));
                }
            }
        }
        Err(format!("variable {} not found", key))
    }

    fn walk_list(&mut self, ctx: &Context, node: &'a ListNode) -> Result<(), String> {
        for n in &node.nodes {
            self.walk(ctx, n)?;
        }
        Ok(())
    }

    // Top level walk function. Steps through the major parts for the template strcuture and
    // writes to the output.
    fn walk(&mut self, ctx: &Context, node: &'a Nodes) -> Result<(), String> {
        self.node = Some(node);
        match *node {
            Nodes::Action(ref n) => {
                let val = self.eval_pipeline(ctx, &n.pipe)?;
                if n.pipe.decl.is_empty() {
                    self.print_value(&val)?;
                }
                Ok(())
            }
            Nodes::If(_) | Nodes::With(_) => self.walk_if_or_with(node, ctx),
            Nodes::Range(ref n) => self.walk_range(ctx, n),
            Nodes::List(ref n) => self.walk_list(ctx, n),
            Nodes::Text(ref n) => write!(self.writer, "{}", n).map_err(|e| format!("{}", e)),
            Nodes::Template(ref n) => self.walk_template(ctx, n),
            _ => Err(format!("unknown node: {}", node)),
        }
    }

    fn walk_template(&mut self, ctx: &Context, template: &TemplateNode) -> Result<(), String> {
        let tree = self.template.tree_set.get(&template.name);
        if let Some(tree) = tree {
            if let Some(ref root) = tree.root {
                let mut vars = VecDeque::new();
                let mut dot = VecDeque::new();
                dot.push_back(Variable {
                    name: "$".to_owned(),
                    value: Arc::clone(&ctx.dot),
                });
                vars.push_back(dot);
                let mut new_state = State {
                    template: self.template,
                    writer: self.writer,
                    node: None,
                    vars,
                    depth: self.depth + 1,
                };
                return new_state.walk(ctx, root);
            }
        }
        Err(String::from("work in progress"))
    }

    fn eval_pipeline(&mut self, ctx: &Context, pipe: &PipeNode) -> Result<Arc<Any>, String> {
        let mut val: Option<Arc<Any>> = None;
        for cmd in &pipe.cmds {
            val = Some(self.eval_command(ctx, cmd, &val)?);
            // TODO
        }
        let val = val.ok_or_else(|| format!("error evaluating pipeline {}", pipe))?;
        for var in &pipe.decl {
            self.vars
                .back_mut()
                .and_then(|v| {
                    Some(v.push_back(Variable {
                        name: var.ident[0].clone(),
                        value: Arc::clone(&val),
                    }))
                })
                .ok_or_else(|| String::from("no stack while evaluating pipeline"))?;
        }
        Ok(val)
    }

    fn eval_command(
        &mut self,
        ctx: &Context,
        cmd: &CommandNode,
        val: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        let first_word = &cmd.args
            .first()
            .ok_or_else(|| format!("no arguments for command node: {}", cmd))?;

        match *(*first_word) {
            Nodes::Field(ref n) => return self.eval_field_node(ctx, n, &cmd.args, val),
            Nodes::Variable(ref n) => return self.eval_variable_node(n, &cmd.args, val),
            Nodes::Pipe(ref n) => return self.eval_pipeline(ctx, n),
            Nodes::Chain(ref n) => return self.eval_chain_node(ctx, n, &cmd.args, val),
            Nodes::Identifier(ref n) => return self.eval_function(ctx, n, &cmd.args, val),
            _ => {}
        }
        not_a_function(&cmd.args, val)?;
        match *(*first_word) {
            Nodes::Bool(ref n) => Ok(Arc::clone(&n.value) as Arc<Any>),
            Nodes::Dot(_) => Ok(Arc::clone(&ctx.dot)),
            Nodes::Number(ref n) => Ok(Arc::clone(&n.value) as Arc<Any>),
            _ => Err(format!("cannot evaluate command {}", first_word)),
        }
    }

    fn eval_function(
        &mut self,
        ctx: &Context,
        ident: &IdentifierNode,
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        let name = &ident.ident;
        let function = self.template
            .funcs
            .get(name.as_str())
            .ok_or_else(|| format!("{} is not a defined function", name))?;
        self.eval_call(ctx, function, args, fin)
    }

    fn eval_call(
        &mut self,
        ctx: &Context,
        function: &Func,
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        let mut arg_vals = vec![];
        for arg in &args[1..] {
            let val = self.eval_arg(ctx, arg)?;
            arg_vals.push(val);
        }
        if let Some(ref f) = *fin {
            arg_vals.push(Arc::clone(f));
        }

        function(&arg_vals)
    }

    fn eval_chain_node(
        &mut self,
        ctx: &Context,
        chain: &ChainNode,
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        if chain.field.is_empty() {
            return Err(String::from("internal error: no fields in eval_chain_node"));
        }
        if let Nodes::Nil(_) = *chain.node {
            return Err(format!("inderection throug explicit nul in {}", chain));
        }
        let pipe = self.eval_arg(ctx, &*chain.node)?;
        self.eval_field_chain(&pipe, &chain.field, args, fin)
    }

    fn eval_arg(&mut self, ctx: &Context, node: &Nodes) -> Result<Arc<Any>, String> {
        match *node {
            Nodes::Dot(_) => Ok(Arc::clone(&ctx.dot)),
            //Nodes::Nil
            Nodes::Field(ref n) => self.eval_field_node(ctx, n, &[], &None), // args?
            Nodes::Variable(ref n) => self.eval_variable_node(n, &[], &None),
            Nodes::Pipe(ref n) => self.eval_pipeline(ctx, n),
            // Nodes::Identifier
            Nodes::Chain(ref n) => self.eval_chain_node(ctx, n, &[], &None),
            Nodes::String(ref n) => Ok(Arc::clone(&n.value) as Arc<Any>),
            Nodes::Bool(ref n) => Ok(Arc::clone(&n.value) as Arc<Any>),
            Nodes::Number(ref n) => Ok(Arc::clone(&n.value) as Arc<Any>),
            _ => Err(format!("cant handle {} as arg", node)),
        }
    }

    fn eval_field_node(
        &mut self,
        ctx: &Context,
        field: &FieldNode,
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        self.eval_field_chain(&ctx.dot, &field.ident, args, fin)
    }

    fn eval_field_chain(
        &mut self,
        receiver: &Arc<Any>,
        ident: &[String],
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        let n = ident.len();
        if n < 1 {
            return Err(String::from("field chain without fields :/"));
        }
        // TODO clean shit up
        let mut r: Arc<Any> = Arc::new(0);
        for (i, id) in ident.iter().enumerate().take(n - 1) {
            r = self.eval_field(if i == 0 { receiver } else { &r }, id, &[], &None)?;
        }
        self.eval_field(if n == 1 { receiver } else { &r }, &ident[n - 1], args, fin)
    }

    fn eval_field(
        &mut self,
        receiver: &Arc<Any>,
        field_name: &str,
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        let has_args = args.len() > 1 || fin.is_some();
        if let Some(val) = receiver.downcast_ref::<Value>() {
            if has_args {
                return Err(format!(
                    "{} has arguments but cannot be invoked as function",
                    field_name
                ));
            }
            return match *val {
                Value::Object(ref o) => o.get(field_name)
                    .map(|v| Arc::new(v.clone()) as Arc<Any>)
                    .ok_or_else(|| format!("no field {} for {}", field_name, val)),
                Value::Map(ref o) => Ok(o.get(field_name)
                    .map(|v| Arc::new(v.clone()) as Arc<Any>)
                    .unwrap_or_else(|| Arc::new(Value::NoValue) as Arc<Any>)),
                _ => Err(String::from("only maps and objects have fields")),
            };
        }

        Err(String::from("only basic fields are supported"))
    }

    fn eval_variable_node(
        &mut self,
        variable: &VariableNode,
        args: &[Nodes],
        fin: &Option<Arc<Any>>,
    ) -> Result<Arc<Any>, String> {
        let val = self.var_value(&variable.ident[0])?;
        if variable.ident.len() == 1 {
            not_a_function(args, fin)?;
            return Ok(val);
        }
        self.eval_field_chain(&val, &variable.ident[1..], args, fin)
    }

    // Walks an `if` or `with` node. They behave the same, except that `wtih` sets dot.
    fn walk_if_or_with(&mut self, node: &'a Nodes, ctx: &Context) -> Result<(), String> {
        let pipe = match *node {
            Nodes::If(ref n) | Nodes::With(ref n) => &n.pipe,
            _ => return Err(format!("expected if or with node, got {}", node)),
        };
        let val = self.eval_pipeline(ctx, pipe)?;
        let truth = is_true(&val);
        if truth {
            match *node {
                Nodes::If(ref n) => self.walk_list(ctx, &n.list)?,
                Nodes::With(ref n) => {
                    let ctx = Context { dot: val };
                    self.walk_list(&ctx, &n.list)?;
                }
                _ => {}
            }
        } else {
            match *node {
                Nodes::If(ref n) | Nodes::With(ref n) => {
                    if let Some(ref otherwise) = n.else_list {
                        self.walk_list(ctx, otherwise)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn one_iteration(
        &mut self,
        key: Value,
        val: Arc<Any>,
        range: &'a RangeNode,
    ) -> Result<(), String> {
        if !range.pipe.decl.is_empty() {
            self.set_kth_last_var_value(1, Arc::clone(&val))?;
        }
        if range.pipe.decl.len() > 1 {
            self.set_kth_last_var_value(2, Arc::new(key))?;
        }
        let vars = VecDeque::new();
        self.vars.push_back(vars);
        let ctx = Context { dot: val };
        self.walk_list(&ctx, &range.list)?;
        self.vars.pop_back();
        Ok(())
    }

    fn walk_range(&mut self, ctx: &Context, range: &'a RangeNode) -> Result<(), String> {
        let val = self.eval_pipeline(ctx, &range.pipe)?;
        if let Some(value) = val.downcast_ref::<Value>() {
            match *value {
                Value::Object(ref map) | Value::Map(ref map) => for (k, v) in map.clone() {
                    self.one_iteration(Value::from(k), Arc::new(v), range)?;
                },
                Value::Array(ref vec) => for (k, v) in vec.iter().enumerate() {
                    self.one_iteration(Value::from(k), Arc::new(v.clone()), range)?;
                },
                _ => return Err(format!("invalid range: {:?}", value)),
            }
        }
        if let Some(ref else_list) = range.else_list {
            self.walk_list(ctx, else_list)?;
        }
        Ok(())
    }

    fn print_value(&mut self, val: &Arc<Any>) -> Result<(), String> {
        print_val!{ val: self <-
                    String,
                    bool,
                    u8,
                    u16,
                    u32,
                    u64,
                    i8,
                    i16,
                    i32,
                    i64,
                    f32,
                    f64,
                    isize,
                    usize,
        };
        if let Some(v) = val.downcast_ref::<Value>() {
            write!(self.writer, "{}", v).map_err(|e| format!("{}", e))?;
            return Ok(());
        }
        Err(String::from("unable to format value"))
    }
}

fn not_a_function(args: &[Nodes], val: &Option<Arc<Any>>) -> Result<(), String> {
    if args.len() > 1 || val.is_some() {
        return Err(format!("can't give arument to non-function {}", args[0]));
    }
    Ok(())
}

#[cfg(test)]
mod tests_mocked {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn simple_template() {
        let data = Context::from(1).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if false }} 2000 {{ end }}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "");

        let data = Context::from(1).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if true }} 2000 {{ end }}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), " 2000 ");

        let data = Context::from(1).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if true -}} 2000 {{- end }}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let data = Context::from(1).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if false -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "3000");
    }

    #[test]
    fn test_dot() {
        let data = Context::from(1).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if . -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let data = Context::from(false).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if . -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "3000");
    }

    #[test]
    fn test_sub() {
        let data = Context::from(1u8).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{.}}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "1");

        #[derive(Gtmpl)]
        struct Foo {
            foo: u8,
        }
        let foo = Foo { foo: 1 };
        let data = Context::from(foo).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{.foo}}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "1");
    }

    #[test]
    fn test_novalue() {
        #[derive(Gtmpl)]
        struct Foo {
            foo: u8,
        }
        let foo = Foo { foo: 1 };
        let data = Context::from(foo).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{.foobar}}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_err());

        let map: HashMap<String, u64> = [("foo".to_owned(), 23u64)].iter().cloned().collect();
        let data = Context::from(map).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{.foo2}}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), Value::NoValue.to_string());
    }

    #[test]
    fn test_dollar_dot() {
        #[derive(Gtmpl, Clone)]
        struct Foo {
            foo: u8,
        }
        let data = Context::from(Foo { foo: 1u8 }).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        println!("{:?}", t.parse(r#"{{$.foo}}"#));
        assert!(t.parse(r#"{{$.foo}}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "1");
    }

    #[test]
    fn test_dot_value() {
        #[derive(Gtmpl, Clone)]
        struct Foo {
            foo: u8,
        }
        #[derive(Gtmpl)]
        struct Bar {
            bar: Foo,
        }
        let foo = Foo { foo: 1 };
        let data = Context::from(foo).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if .foo -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let foo = Foo { foo: 0 };
        let data = Context::from(foo).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if .foo -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "3000");

        let bar = Bar {
            bar: Foo { foo: 1 },
        };
        let data = Context::from(bar).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if .bar.foo -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let bar = Bar {
            bar: Foo { foo: 0 },
        };
        let data = Context::from(bar).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if .bar.foo -}} 2000 {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "3000");
    }

    #[test]
    fn test_with() {
        #[derive(Gtmpl)]
        struct Foo {
            foo: u16,
        }
        let foo = Foo { foo: 1000 };
        let data = Context::from(foo).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ with .foo -}} {{.}} {{- else -}} 3000 {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "1000");
    }

    fn to_sorted_string(buf: Vec<u8>) -> String {
        let mut chars: Vec<char> = String::from_utf8(buf).unwrap().chars().collect();
        chars.sort();
        chars.iter().cloned().collect::<String>()
    }

    #[test]
    fn test_range() {
        let mut map = HashMap::new();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);
        let data = Context::from(map).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ range . -}} {{.}} {{- end }}"#).is_ok());
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(to_sorted_string(w), "12");

        let vec = vec!["foo", "bar", "2000"];
        let data = Context::from(vec).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ range . -}} {{.}} {{- end }}"#).is_ok());
        let out = t.execute(&mut w, &data);
        println!("{:?}", out);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "foobar2000");
    }

    #[test]
    fn test_proper_range() {
        let mut map = HashMap::new();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);
        let data = Context::from(map).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ range $k, $v := . -}} {{ $v }} {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(to_sorted_string(w), "12");

        let mut map = HashMap::new();
        map.insert("a".to_owned(), "b");
        map.insert("c".to_owned(), "d");
        let data = Context::from(map).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ range $k, $v := . -}} {{ $k }}{{ $v }} {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(to_sorted_string(w), "abcd");

        let mut map = HashMap::new();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);
        let data = Context::from(map).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ range $k, $v := . -}} {{ $k }}{{ $v }} {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(to_sorted_string(w), "12ab");

        let mut map = HashMap::new();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);
        #[derive(Gtmpl)]
        struct Foo {
            foo: HashMap<String, i32>,
        }
        let foo = Foo { foo: map };
        let data = Context::from(foo).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ range $k, $v := .foo -}} {{ $v }} {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(to_sorted_string(w), "12");

        let mut map = HashMap::new();
        #[derive(Gtmpl, Clone)]
        struct Bar {
            bar: i32,
        }
        map.insert("a".to_owned(), Bar { bar: 1 });
        map.insert("b".to_owned(), Bar { bar: 2 });
        let data = Context::from(map).unwrap();
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ range $k, $v := . -}} {{ $v.bar }} {{- end }}"#)
                .is_ok()
        );
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(to_sorted_string(w), "12");
    }

    #[test]
    fn test_len() {
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"my len is {{ len . }}"#).is_ok());
        let data = Context::from(vec![1, 2, 3]).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "my len is 3");

        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ len . }}"#).is_ok());
        let data = Context::from("hello".to_owned()).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "5");
    }

    #[test]
    fn test_pipeline_function() {
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if ( 1 | eq . ) -}} 2000 {{- end }}"#).is_ok());
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");
    }

    #[test]
    fn test_function() {
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if eq . . -}} 2000 {{- end }}"#).is_ok());
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");
    }

    #[test]
    fn test_eq() {
        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if eq "a" "a" -}} 2000 {{- end }}"#).is_ok());
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if eq "a" "b" -}} 2000 {{- end }}"#).is_ok());
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "");

        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if eq true true -}} 2000 {{- end }}"#).is_ok());
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if eq true false -}} 2000 {{- end }}"#)
                .is_ok()
        );
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "");

        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(
            t.parse(r#"{{ if eq 23.42 23.42 -}} 2000 {{- end }}"#)
                .is_ok()
        );
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");

        let mut w: Vec<u8> = vec![];
        let mut t = Template::default();
        assert!(t.parse(r#"{{ if eq 1 . -}} 2000 {{- end }}"#).is_ok());
        let data = Context::from(1).unwrap();
        let out = t.execute(&mut w, &data);
        assert!(out.is_ok());
        assert_eq!(String::from_utf8(w).unwrap(), "2000");
    }
}
