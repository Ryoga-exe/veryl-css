use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use veryl_analyzer::ir::{
    AssignDestination, AssignStatement, Component, Declaration, Expression, Factor,
    IfResetStatement, IfStatement, Ir, Module, Op, Statement, VarId, VarKind, Variable,
};

pub struct Output {
    pub css: String,
}

pub fn emit(ir: &Ir) -> Result<Output> {
    let modules: Vec<&Module> = ir
        .components
        .iter()
        .filter_map(|c| if let Component::Module(m) = c { Some(m) } else { None })
        .collect();

    match modules.as_slice() {
        [module] => emit_module(module),
        _ => bail!("expected exactly one top module, found {}", modules.len()),
    }
}

// ── FF register helper ────────────────────────────────────────────────────────

struct FfRegInfo {
    current: String,   // --o or --Mod-t  (public; read alias)
    next: String,      // --Mod-o-next    (unregistered; next-state)
    captured: String,  // --Mod-o-captured (unregistered)
    hoist: String,     // --Mod-o-hoist   (unregistered)
}

// ── Emit context (threaded through statement emitters) ────────────────────────

/// Mutable state threaded through all statement/expression emitters.
struct Ctx<'a> {
    /// VarId → CSS custom property name (public signals only)
    names: &'a BTreeMap<VarId, String>,
    /// Counter for unique condition temp variable names (--veryl-cond-N)
    cond_counter: usize,
    /// Which comparison @functions are actually used (determines what to emit)
    used_comp_fns: BTreeSet<CompFn>,
}

impl<'a> Ctx<'a> {
    fn new(names: &'a BTreeMap<VarId, String>) -> Self {
        Self { names, cond_counter: 0, used_comp_fns: BTreeSet::new() }
    }

    /// Allocate a fresh condition temporary CSS variable name.
    fn next_cond_var(&mut self) -> String {
        let n = self.cond_counter;
        self.cond_counter += 1;
        format!("--veryl-cond-{n}")
    }
}

/// Which comparison @function is needed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CompFn { Lt, Lte, Gt, Gte, Eq, Neq }

// ── Module emission ───────────────────────────────────────────────────────────

fn emit_module(module: &Module) -> Result<Output> {
    let module_name = module.name.to_string();

    // Pass 1: collect FF metadata (clock/reset ids, FF-assigned variable ids)
    let mut clock_ids = BTreeSet::<VarId>::new();
    let mut reset_ids = BTreeSet::<VarId>::new();
    let mut ff_assigned_ids = BTreeSet::<VarId>::new();

    for decl in &module.declarations {
        if let Declaration::Ff(ff) = decl {
            clock_ids.insert(ff.clock.id);
            if let Some(r) = &ff.reset {
                reset_ids.insert(r.id);
            }
            collect_assigned_ids(&ff.statements, &mut ff_assigned_ids);
        }
    }

    let has_ff = !clock_ids.is_empty();

    // Pass 2: build variable maps.
    //
    // Only *public* signals (ports + internal logic vars) get @property declarations.
    // Intermediate animation variables (-hoist, -captured, -next) are intentionally
    // left unregistered: CSS must treat them as opaque strings and snapshot them at
    // keyframe boundaries rather than interpolating them as typed values.
    let mut names = BTreeMap::<VarId, String>::new();
    let mut ff_regs = BTreeMap::<VarId, FfRegInfo>::new();
    let mut public_css_vars = BTreeSet::<String>::new();

    for (&id, variable) in &module.variables {
        if clock_ids.contains(&id) {
            continue; // clock ports have no CSS representation
        }

        let is_reset = reset_ids.contains(&id);
        if !is_reset {
            ensure_i32_like(variable)?;
        }

        let Some(css_name) = variable_css_name(variable, &module_name)? else {
            continue; // Param / Const — excluded from CSS
        };

        if ff_assigned_ids.contains(&id) && !is_reset {
            let signal = last_path_segment(&variable.path.to_string())?;
            let base = format!("--{module_name}-{signal}");
            ff_regs.insert(id, FfRegInfo {
                current: css_name.clone(),
                next: format!("{base}-next"),
                captured: format!("{base}-captured"),
                hoist: format!("{base}-hoist"),
            });
        }

        names.insert(id, css_name.clone());
        public_css_vars.insert(css_name);
    }

    // Pass 3: emit statements into line buffers
    let ff_next_names: BTreeMap<VarId, String> =
        ff_regs.iter().map(|(id, i)| (*id, i.next.clone())).collect();

    let mut ctx = Ctx::new(&names);

    let mut ff_current_lines = Vec::<String>::new();
    let mut comb_lines = Vec::<String>::new();
    let mut ff_next_lines = Vec::<String>::new();
    let mut saw_comb = false;

    for info in ff_regs.values() {
        // Public alias reads from the hoisted value; defaults to 0 before first cycle
        ff_current_lines.push(format!("{}: var({}, 0);", info.current, info.hoist));
    }

    for decl in &module.declarations {
        match decl {
            Declaration::Comb(comb) => {
                saw_comb = true;
                for stmt in &comb.statements {
                    emit_comb_statement(stmt, &mut ctx, &mut comb_lines)?;
                }
            }
            Declaration::Ff(ff) => {
                for stmt in &ff.statements {
                    emit_ff_statement(stmt, &mut ctx, &ff_next_names, &mut ff_next_lines)?;
                }
            }
            Declaration::Null => {}
            other => bail!("unsupported declaration: {other}"),
        }
    }

    if !has_ff && !saw_comb {
        bail!("no comb or ff declaration found");
    }

    // Pass 4: assemble CSS output
    let mut css = String::from("/* generated by veryl-css */\n\n");

    // @function declarations for comparison operators (only if used)
    if !ctx.used_comp_fns.is_empty() {
        css.push_str(&css_comp_functions(&ctx.used_comp_fns));
    }

    for var in &public_css_vars {
        css.push_str(&css_property(var));
    }

    if has_ff {
        // Computation lives on `body` — the same element as the user's clock animations.
        // CSS inheritance is parent→child only; :root cannot read values written by
        // animations on body, so computation and animation must be on the same element.
        let body_lines = ff_current_lines.iter()
            .chain(&comb_lines)
            .chain(&ff_next_lines);
        css.push_str(&css_block("body", body_lines));

        // @keyframes for the two-phase FF state transfer.
        // The `body { animation: hoist ...; }` setup and phase-control rules are NOT
        // emitted here — they belong in the user's HTML as clock infrastructure.
        let mut hoist_inner = Vec::<String>::new();
        let mut capture_inner = Vec::<String>::new();
        for info in ff_regs.values() {
            hoist_inner.push(format!("{}: var({}, 0);", info.hoist, info.captured));
            capture_inner.push(format!("{}: var({});", info.captured, info.next));
        }
        css.push_str(&css_keyframes("hoist", &hoist_inner));
        css.push_str(&css_keyframes("capture", &capture_inner));
    } else {
        css.push_str(&css_block(":root", &comb_lines));
    }

    Ok(Output { css })
}

// ── FF helpers ────────────────────────────────────────────────────────────────

/// Collect all variable ids that appear on the left-hand side of assignments
/// anywhere within the given statement list (recursing into if/if_reset branches).
fn collect_assigned_ids(statements: &[Statement], ids: &mut BTreeSet<VarId>) {
    for stmt in statements {
        match stmt {
            Statement::Assign(a) => ids.extend(a.dst.iter().map(|d| d.id)),
            Statement::If(s) => {
                collect_assigned_ids(&s.true_side, ids);
                collect_assigned_ids(&s.false_side, ids);
            }
            Statement::IfReset(s) => {
                collect_assigned_ids(&s.true_side, ids);
                collect_assigned_ids(&s.false_side, ids);
            }
            _ => {}
        }
    }
}

fn emit_ff_statement(
    statement: &Statement,
    ctx: &mut Ctx,
    ff_next_names: &BTreeMap<VarId, String>,
    out: &mut Vec<String>,
) -> Result<()> {
    match statement {
        Statement::Assign(assign) => {
            let (dst, expr) = unpack_assign_expr(assign, ctx.names)?;
            let next = ff_next_names
                .get(&dst.id)
                .ok_or_else(|| anyhow!("ff assignment to non-ff variable: {}", dst.id))?;
            out.push(format!("{next}: {expr};"));
            Ok(())
        }
        Statement::If(if_stmt) => {
            let mut preamble = Vec::new();
            let vals = eval_if_expr(if_stmt, ctx, &mut preamble)?;
            out.extend(preamble);
            for (id, expr) in vals {
                let next = ff_next_names
                    .get(&id)
                    .ok_or_else(|| anyhow!("if assigns to non-ff variable: {id}"))?;
                out.push(format!("{next}: {expr};"));
            }
            Ok(())
        }
        Statement::IfReset(if_reset) => emit_if_reset(if_reset, ctx, ff_next_names, out),
        Statement::Null => Ok(()),
        other => bail!("unsupported ff statement: {other}"),
    }
}

fn emit_if_reset(
    if_reset: &IfResetStatement,
    ctx: &mut Ctx,
    ff_next_names: &BTreeMap<VarId, String>,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut preamble = Vec::new();

    // Reset branch: only simple assigns expected (by Veryl convention)
    let reset_exprs = eval_branch(&if_reset.true_side, ctx, &mut preamble)?;
    // Normal branch: may contain nested `if` statements
    let normal_exprs = eval_branch(&if_reset.false_side, ctx, &mut preamble)?;

    out.extend(preamble);

    let all_ids: BTreeSet<VarId> =
        reset_exprs.keys().chain(normal_exprs.keys()).cloned().collect();

    for id in all_ids {
        let next = ff_next_names
            .get(&id)
            .ok_or_else(|| anyhow!("if_reset assigns to non-ff variable: {id}"))?;

        let line = match (reset_exprs.get(&id), normal_exprs.get(&id)) {
            (Some(rv), Some(nv)) =>
                format!("{next}: if(style(--rst: 1): {rv}; else: {nv});"),
            (Some(rv), None) =>
                format!("{next}: {rv};"),
            (None, Some(nv)) =>
                format!("{next}: {nv};"),
            (None, None) => unreachable!(),
        };
        out.push(line);
    }

    Ok(())
}

/// Recursively evaluate a branch's statements into `{ VarId → CSS value expression }`.
///
/// Nested `if` statements are inlined as `if(style(--cond-N: 1): ...; else: ...)` expressions.
/// Condition temp vars are pushed to `preamble` in evaluation order.
fn eval_branch(
    stmts: &[Statement],
    ctx: &mut Ctx,
    preamble: &mut Vec<String>,
) -> Result<BTreeMap<VarId, String>> {
    let mut map = BTreeMap::new();
    for stmt in stmts {
        match stmt {
            Statement::Assign(a) => {
                let (dst, expr) = unpack_assign_expr(a, ctx.names)?;
                map.insert(dst.id, expr);
            }
            Statement::If(if_stmt) => {
                let nested = eval_if_expr(if_stmt, ctx, preamble)?;
                map.extend(nested);
            }
            Statement::Null => {}
            other => bail!("unsupported statement in branch: {other}"),
        }
    }
    Ok(map)
}

/// Evaluate an `if` statement into `{ VarId → CSS value expression }`.
///
/// Emits condition temp var into `preamble`; recurses for nested ifs.
fn eval_if_expr(
    if_stmt: &IfStatement,
    ctx: &mut Ctx,
    preamble: &mut Vec<String>,
) -> Result<BTreeMap<VarId, String>> {
    let cond_var = ctx.next_cond_var();
    let cond_css = emit_condition(&if_stmt.cond, ctx)?;
    preamble.push(format!("{cond_var}: {cond_css};"));

    let true_exprs = eval_branch(&if_stmt.true_side, ctx, preamble)?;
    let false_exprs = eval_branch(&if_stmt.false_side, ctx, preamble)?;

    let all_ids: BTreeSet<VarId> =
        true_exprs.keys().chain(false_exprs.keys()).cloned().collect();

    let mut result = BTreeMap::new();
    for id in all_ids {
        let true_val = true_exprs.get(&id)
            .ok_or_else(|| anyhow!("variable {id} not assigned in true branch of if"))?;
        let false_val = false_exprs.get(&id)
            .ok_or_else(|| anyhow!("variable {id} not assigned in false branch of if"))?;
        result.insert(id, format!("if(style({cond_var}: 1): {true_val}; else: {false_val})"));
    }
    Ok(result)
}

// ── always_comb statement emitter ─────────────────────────────────────────────

fn emit_comb_statement(
    statement: &Statement,
    ctx: &mut Ctx,
    out: &mut Vec<String>,
) -> Result<()> {
    match statement {
        Statement::Assign(assign) => {
            let (dst, expr) = unpack_assign_expr(assign, ctx.names)?;
            let dst_name = ctx.names
                .get(&dst.id)
                .ok_or_else(|| anyhow!("unknown assignment destination: {}", dst.id))?;
            out.push(format!("{dst_name}: {expr};"));
            Ok(())
        }
        Statement::If(if_stmt) => emit_comb_if(if_stmt, ctx, out),
        Statement::Null => Ok(()),
        other => bail!("unsupported statement: {other}"),
    }
}

/// Emit a Veryl `if` statement inside `always_comb`.
fn emit_comb_if(
    if_stmt: &IfStatement,
    ctx: &mut Ctx,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut preamble = Vec::new();
    let vals = eval_if_expr(if_stmt, ctx, &mut preamble)?;
    out.extend(preamble);
    for (id, expr) in vals {
        let dst_name = ctx.names
            .get(&id)
            .ok_or_else(|| anyhow!("unknown variable in if branch: {id}"))?;
        out.push(format!("{dst_name}: {expr};"));
    }
    Ok(())
}

// ── Condition expression emitter ──────────────────────────────────────────────

/// Emit a condition expression as a CSS value that resolves to 0 or 1.
///
/// For comparison operators, calls the corresponding `@function`.
/// Records which functions are needed in `ctx.used_comp_fns`.
fn emit_condition(expr: &Expression, ctx: &mut Ctx) -> Result<String> {
    match expr {
        Expression::Binary(lhs, op, rhs, _) => {
            let l = emit_expr(lhs, ctx.names)?;
            let r = emit_expr(rhs, ctx.names)?;
            let (fn_name, comp) = match op {
                Op::Less     => ("--veryl-lt",  CompFn::Lt),
                Op::LessEq   => ("--veryl-lte", CompFn::Lte),
                Op::Greater  => ("--veryl-gt",  CompFn::Gt),
                Op::GreaterEq => ("--veryl-gte", CompFn::Gte),
                Op::Eq       => ("--veryl-eq",  CompFn::Eq),
                Op::Ne       => ("--veryl-neq", CompFn::Neq),
                _ => bail!("unsupported condition operator: {op}"),
            };
            ctx.used_comp_fns.insert(comp);
            Ok(format!("{fn_name}({l}, {r})"))
        }
        other => bail!("unsupported condition expression: {other}"),
    }
}

// ── Expression emitters ───────────────────────────────────────────────────────

fn emit_expr(expr: &Expression, names: &BTreeMap<VarId, String>) -> Result<String> {
    match expr {
        Expression::Term(factor) => emit_factor(factor, names),
        Expression::Unary(op, inner, _) => {
            let inner = emit_expr(inner, names)?;
            match op {
                Op::Add => Ok(inner),
                Op::Sub => Ok(format!("calc(-1 * ({inner}))")),
                _ => bail!("unsupported unary operator: {op}"),
            }
        }
        Expression::Binary(lhs, op, rhs, _) => {
            let lhs = emit_expr(lhs, names)?;
            let rhs = emit_expr(rhs, names)?;
            emit_binary(op, &lhs, &rhs)
        }
        other => bail!("unsupported expression: {other}"),
    }
}

fn emit_factor(factor: &Factor, names: &BTreeMap<VarId, String>) -> Result<String> {
    match factor {
        Factor::Variable(id, index, select, _) => {
            if index.dimension() != 0 {
                bail!("array indexing is unsupported");
            }
            if !select.to_string().is_empty() {
                bail!("bit/part-select is unsupported");
            }
            let css_name = names
                .get(id)
                .ok_or_else(|| anyhow!("unknown variable id: {id}"))?;
            Ok(format!("var({css_name})"))
        }
        Factor::Value(comptime) => {
            let value = comptime
                .get_value()
                .map_err(|_| anyhow!("failed to evaluate literal value"))?;
            // LowerHex on Value emits full Veryl literal format: e.g. "32'sh00000001"
            literal_to_css_int(&format!("{value:x}"))
        }
        other => bail!("unsupported factor: {other}"),
    }
}

fn emit_binary(op: &Op, lhs: &str, rhs: &str) -> Result<String> {
    Ok(match op {
        Op::Add => format!("calc(({lhs}) + ({rhs}))"),
        Op::Sub => format!("calc(({lhs}) - ({rhs}))"),
        Op::Mul => format!("calc(({lhs}) * ({rhs}))"),
        Op::Div => format!("round(to-zero, calc(({lhs}) / ({rhs})), 1)"),
        Op::Rem => format!("rem(({lhs}), ({rhs}))"),
        _ => bail!("unsupported binary operator: {op}"),
    })
}

// ── Small reusable helpers ────────────────────────────────────────────────────

/// Extract (dst, emitted_expr) from a single-destination assign, or bail.
fn unpack_assign_expr<'a>(
    assign: &'a AssignStatement,
    names: &BTreeMap<VarId, String>,
) -> Result<(&'a AssignDestination, String)> {
    if assign.dst.len() != 1 {
        bail!("only single-destination assignments are supported");
    }
    let dst = &assign.dst[0];
    if dst.index.dimension() != 0 {
        bail!("array destination is unsupported");
    }
    if !dst.select.to_string().is_empty() {
        bail!("bit/part-select destination is unsupported");
    }
    let expr = emit_expr(&assign.expr, names)?;
    Ok((dst, expr))
}

// ── CSS output helpers ────────────────────────────────────────────────────────

fn css_comp_functions(used: &BTreeSet<CompFn>) -> String {
    // @function uses <integer> since the result is always 0 or 1.
    let all: &[(CompFn, &str, &str)] = &[
        (CompFn::Lt,  "--veryl-lt",  "clamp(0, sign(calc(var(--b) - var(--a))), 1)"),
        (CompFn::Lte, "--veryl-lte", "clamp(0, calc(1 + sign(calc(var(--b) - var(--a)))), 1)"),
        (CompFn::Gt,  "--veryl-gt",  "clamp(0, sign(calc(var(--a) - var(--b))), 1)"),
        (CompFn::Gte, "--veryl-gte", "clamp(0, calc(1 + sign(calc(var(--a) - var(--b)))), 1)"),
        (CompFn::Eq,  "--veryl-eq",  "clamp(0, calc(1 - abs(sign(calc(var(--a) - var(--b))))), 1)"),
        (CompFn::Neq, "--veryl-neq", "abs(sign(calc(var(--a) - var(--b))))"),
    ];
    let mut s = String::new();
    for (tag, name, body) in all {
        if used.contains(tag) {
            s.push_str(&format!(
                "@function {name}(--a <number>, --b <number>) returns <integer> {{\n  result: {body};\n}}\n\n"
            ));
        }
    }
    s
}

fn css_property(var: &str) -> String {
    format!(
        "@property {var} {{\n  syntax: \"<integer>\";\n  inherits: true;\n  initial-value: 0;\n}}\n\n"
    )
}

fn css_block<'a>(selector: &str, lines: impl IntoIterator<Item = &'a String>) -> String {
    let mut s = format!("{selector} {{\n");
    for line in lines {
        s.push_str(&format!("  {line}\n"));
    }
    s.push_str("}\n\n");
    s
}

fn css_keyframes(name: &str, lines: &[String]) -> String {
    let inner = lines.iter().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n");
    format!("@keyframes {name} {{\n  0%, 100% {{\n{inner}\n  }}\n}}\n\n")
}

// ── Variable helpers ──────────────────────────────────────────────────────────

fn variable_css_name(variable: &Variable, module_name: &str) -> Result<Option<String>> {
    let signal = last_path_segment(&variable.path.to_string())?;
    Ok(match variable.kind {
        VarKind::Input | VarKind::Output | VarKind::Inout => Some(format!("--{signal}")),
        VarKind::Variable | VarKind::Let => Some(format!("--{module_name}-{signal}")),
        VarKind::Param | VarKind::Const => None,
    })
}

fn ensure_i32_like(variable: &Variable) -> Result<()> {
    let ty = variable.r#type.to_string();
    if ty == "signed bit<32>" {
        Ok(())
    } else {
        bail!(
            "only `signed bit<32>` variables are supported, got `{}` for `{}`",
            ty,
            variable.path
        )
    }
}

fn last_path_segment(path: &str) -> Result<String> {
    path.rsplit('.')
        .next()
        .map(|x| x.to_string())
        .ok_or_else(|| anyhow!("empty path"))
}

/// Parse a Veryl literal or plain decimal/signed integer string into a CSS integer string.
///
/// Accepts:
/// - Plain decimal: `"42"`, `"-1"`
/// - Veryl literal: `"32'sh00000001"`, `"8'b10101010"`, etc.
///
/// Rejects 4-value logic (x/z) literals.
fn literal_to_css_int(text: &str) -> Result<String> {
    let normalized = text.trim().replace('_', "");
    let lower = normalized.to_ascii_lowercase();

    if lower.contains('x') || lower.contains('z') {
        bail!("x/z literal is unsupported: {text}");
    }

    if let Some((width_str, rhs)) = lower.split_once('\'') {
        // Veryl literal: [width]'[s]<base><digits>
        let signed = rhs.starts_with('s');
        let rest = if signed { &rhs[1..] } else { rhs };
        let base_ch = rest.chars().next().unwrap_or_default();
        let digits = &rest[1..];
        let width: Option<u32> = width_str.parse().ok();

        // Parse as unsigned first, then sign-extend if the literal is signed
        let unsigned_val: u128 = match base_ch {
            'h' => u128::from_str_radix(digits, 16)?,
            'd' => digits.parse()?,
            'b' => u128::from_str_radix(digits, 2)?,
            'o' => u128::from_str_radix(digits, 8)?,
            _ => bail!("unsupported literal base in: {text}"),
        };

        let value: i128 = if signed {
            // Sign-extend: if the MSB within `width` bits is 1, the value is negative
            if let Some(w) = width.filter(|&w| w > 0 && w <= 128) {
                let msb = (unsigned_val >> (w - 1)) & 1;
                if msb == 1 {
                    (unsigned_val | (u128::MAX << w)) as i128
                } else {
                    unsigned_val as i128
                }
            } else {
                unsigned_val as i128
            }
        } else {
            unsigned_val as i128
        };

        return Ok(value.to_string());
    }

    // Plain decimal (possibly negative)
    if lower.starts_with('-') || lower.chars().all(|c| c.is_ascii_digit()) {
        return Ok(lower);
    }

    bail!("unsupported literal syntax: {text}")
}
