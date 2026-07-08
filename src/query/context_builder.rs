use crate::models::{CorpusType, CorpusWeights, SearchResult};

pub struct ContextBuilder;

impl ContextBuilder {
    /// Format static and dynamic chunks into LLM-ready context string.
    pub fn build_context(
        static_chunks: &[SearchResult],
        dynamic_chunks: &[SearchResult],
        query: &str,
    ) -> String {
        let mut out = String::from("=== 参考资料 ===\n\n");

        for (i, r) in static_chunks.iter().enumerate() {
            out.push_str(&static_chunk_block(i, r));
        }

        for (i, r) in dynamic_chunks.iter().enumerate() {
            out.push_str(&dynamic_chunk_block(i, r));
        }

        out.push_str(&format!("=== 问题 ===\n{query}"));
        out
    }

    /// Like `build_context`, but drops lowest-ranked chunks until the estimated
    /// token count fits within `max_tokens`.
    ///
    /// Chunks are dropped from the tail (lowest rank) of whichever group
    /// currently costs more tokens, so both groups shrink roughly evenly and
    /// within-group ranking is preserved.
    pub fn build_context_bounded(
        static_chunks: &[SearchResult],
        dynamic_chunks: &[SearchResult],
        query: &str,
        max_tokens: usize,
    ) -> String {
        let fixed = Self::estimate_tokens(&Self::build_context(&[], &[], query));
        let static_costs: Vec<usize> = static_chunks
            .iter()
            .enumerate()
            .map(|(i, r)| Self::estimate_tokens(&static_chunk_block(i, r)))
            .collect();
        let dynamic_costs: Vec<usize> = dynamic_chunks
            .iter()
            .enumerate()
            .map(|(i, r)| Self::estimate_tokens(&dynamic_chunk_block(i, r)))
            .collect();

        let mut kept_static = static_chunks.len();
        let mut kept_dynamic = dynamic_chunks.len();
        let mut static_total: usize = static_costs.iter().sum();
        let mut dynamic_total: usize = dynamic_costs.iter().sum();

        while fixed + static_total + dynamic_total > max_tokens
            && (kept_static > 0 || kept_dynamic > 0)
        {
            let drop_static = if kept_dynamic == 0 {
                true
            } else if kept_static == 0 {
                false
            } else if static_total != dynamic_total {
                static_total > dynamic_total
            } else {
                kept_static > kept_dynamic
            };
            if drop_static {
                kept_static -= 1;
                static_total -= static_costs[kept_static];
            } else {
                kept_dynamic -= 1;
                dynamic_total -= dynamic_costs[kept_dynamic];
            }
        }

        Self::build_context(
            &static_chunks[..kept_static],
            &dynamic_chunks[..kept_dynamic],
            query,
        )
    }

    /// Estimate LLM token count for `text`: CJK characters count as one token
    /// each, everything else as one token per 4 characters. Per-block estimates
    /// sum to at least the whole-string estimate, so budgeting by blocks never
    /// underestimates.
    pub fn estimate_tokens(text: &str) -> usize {
        let (cjk, other) = text.chars().fold((0usize, 0usize), |(cjk, other), c| {
            if is_cjk(c) {
                (cjk + 1, other)
            } else {
                (cjk, other + 1)
            }
        });
        cjk + other.div_ceil(4)
    }

    /// Build system prompt with weight instruction based on corpus_weights.
    pub fn build_system_prompt(weights: Option<&CorpusWeights>) -> String {
        let weight_instruction = weight_instruction(weights);
        format!(
            "你是一个专业的文档检索助手。\n\
             \n\
             参考资料分为两类：\n\
             - [法规/合同/RFC]：来自共享权威文档库（法律法规、合同模板、RFC等）\n\
             - [用户数据]：来自用户的私有文档\n\
             \n\
             {weight_instruction}\n\
             \n\
             回答时只引用与问题直接相关的内容，忽略无关片段。\n\
             引用时注明来源类型。"
        )
    }
}

fn weight_instruction(weights: Option<&CorpusWeights>) -> &'static str {
    match weights {
        Some(w) if w.static_bias > 0.7 => "如法规/合同与用户数据冲突，以法规/合同为准。",
        Some(w) if w.dynamic_bias > 0.7 => "优先参考用户数据，不足时补充引用法规/合同。",
        _ => "综合两类来源回答，不偏向任何一方。",
    }
}

pub(crate) fn corpus_type_label(ct: Option<&CorpusType>) -> &'static str {
    match ct {
        Some(CorpusType::Legal) => "法规",
        Some(CorpusType::Contract) => "合同",
        Some(CorpusType::Rfc) => "RFC",
        _ => "法规/合同",
    }
}

fn static_chunk_block(index: usize, r: &SearchResult) -> String {
    chunk_block(corpus_type_label(r.corpus_type.as_ref()), index, r)
}

fn dynamic_chunk_block(index: usize, r: &SearchResult) -> String {
    chunk_block("用户数据", index, r)
}

fn chunk_block(label: &str, index: usize, r: &SearchResult) -> String {
    let n = index + 1;
    match r.citation.as_ref().and_then(|c| c.title.as_deref()) {
        Some(title) => format!("[{label} #{n}] {title}\n{}\n\n", r.text),
        None => format!("[{label} #{n}]\n{}\n\n", r.text),
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        // CJK punctuation, CJK Ext A, CJK Unified, compatibility ideographs, fullwidth
        // forms, and supplementary ideographic planes (Ext B..G + compat supplement)
        '\u{3000}'..='\u{303F}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FF00}'..='\u{FFEF}'
            | '\u{20000}'..='\u{2FA1F}'
            | '\u{30000}'..='\u{3134F}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChunkSource, Citation, SearchSource};

    fn make_result(text: &str, chunk_source: ChunkSource, ct: Option<CorpusType>) -> SearchResult {
        SearchResult {
            doc_id: "1".into(),
            score: 0.9,
            text: text.into(),
            metadata: None,
            source: SearchSource::Vector,
            chunk_source,
            corpus_type: ct,
            citation: None,
        }
    }

    #[test]
    fn context_contains_section_headers() {
        let static_r = make_result("law text", ChunkSource::Static, Some(CorpusType::Legal));
        let dynamic_r = make_result("user doc", ChunkSource::Dynamic, None);
        let ctx = ContextBuilder::build_context(&[static_r], &[dynamic_r], "what is x?");
        assert!(ctx.contains("=== 参考资料 ==="));
        assert!(ctx.contains("[法规 #1]"));
        assert!(ctx.contains("law text"));
        assert!(ctx.contains("[用户数据 #1]"));
        assert!(ctx.contains("user doc"));
        assert!(ctx.contains("=== 问题 ==="));
        assert!(ctx.contains("what is x?"));
    }

    #[test]
    fn weight_instruction_static_bias() {
        let w = CorpusWeights {
            static_bias: 0.9,
            dynamic_bias: 0.1,
        };
        let prompt = ContextBuilder::build_system_prompt(Some(&w));
        assert!(prompt.contains("以法规/合同为准"));
    }

    #[test]
    fn weight_instruction_dynamic_bias() {
        let w = CorpusWeights {
            static_bias: 0.1,
            dynamic_bias: 0.9,
        };
        let prompt = ContextBuilder::build_system_prompt(Some(&w));
        assert!(prompt.contains("优先参考用户数据"));
    }

    #[test]
    fn weight_instruction_default() {
        let prompt = ContextBuilder::build_system_prompt(None);
        assert!(prompt.contains("不偏向任何一方"));
    }

    #[test]
    fn empty_chunks_produces_valid_context() {
        let ctx = ContextBuilder::build_context(&[], &[], "test?");
        assert!(ctx.contains("=== 问题 ==="));
        assert!(ctx.contains("test?"));
    }

    fn make_result_with_title(
        text: &str,
        chunk_source: ChunkSource,
        ct: Option<CorpusType>,
        title: Option<&str>,
    ) -> SearchResult {
        let mut r = make_result(text, chunk_source, ct);
        r.citation = Some(Citation {
            resource_id: "res-1".into(),
            source_type: "s3".into(),
            source_ref: "ref-1".into(),
            title: title.map(Into::into),
            url: None,
        });
        r
    }

    #[test]
    fn citation_title_rendered_after_label() {
        let static_r = make_result_with_title(
            "law text",
            ChunkSource::Static,
            Some(CorpusType::Legal),
            Some("民法典"),
        );
        let dynamic_r =
            make_result_with_title("user doc", ChunkSource::Dynamic, None, Some("会议记录"));
        let ctx = ContextBuilder::build_context(&[static_r], &[dynamic_r], "what is x?");
        assert!(ctx.contains("[法规 #1] 民法典\nlaw text"));
        assert!(ctx.contains("[用户数据 #1] 会议记录\nuser doc"));
    }

    #[test]
    fn missing_title_keeps_bare_label() {
        let static_r = make_result_with_title(
            "law text",
            ChunkSource::Static,
            Some(CorpusType::Legal),
            None,
        );
        let dynamic_r = make_result("user doc", ChunkSource::Dynamic, None);
        let ctx = ContextBuilder::build_context(&[static_r], &[dynamic_r], "what is x?");
        assert!(ctx.contains("[法规 #1]\nlaw text"));
        assert!(ctx.contains("[用户数据 #1]\nuser doc"));
    }

    #[test]
    fn estimate_tokens_cjk_counts_one_per_char() {
        assert_eq!(ContextBuilder::estimate_tokens("你好世界"), 4);
    }

    #[test]
    fn estimate_tokens_ascii_counts_four_chars_per_token() {
        assert_eq!(ContextBuilder::estimate_tokens("hello world"), 3);
    }

    #[test]
    fn estimate_tokens_mixed_text() {
        assert_eq!(ContextBuilder::estimate_tokens("合同termination条款"), 7);
    }

    #[test]
    fn estimate_tokens_cjk_extension_b_counts_one_per_char() {
        // U+20000..U+20003, CJK Extension B (rare ideographs in names)
        assert_eq!(ContextBuilder::estimate_tokens("𠀀𠀁𠀂𠀃"), 4);
    }

    #[test]
    fn bounded_equals_unbounded_when_budget_ample() {
        let static_r = make_result("law text", ChunkSource::Static, Some(CorpusType::Legal));
        let dynamic_r = make_result("user doc", ChunkSource::Dynamic, None);
        let statics = [static_r];
        let dynamics = [dynamic_r];
        let unbounded = ContextBuilder::build_context(&statics, &dynamics, "what is x?");
        let bounded =
            ContextBuilder::build_context_bounded(&statics, &dynamics, "what is x?", usize::MAX);
        assert_eq!(bounded, unbounded);
    }

    #[test]
    fn bounded_drops_tail_chunks_when_over_budget() {
        let padding = "a".repeat(192);
        let statics: Vec<SearchResult> = (1..=4)
            .map(|i| {
                make_result(
                    &format!("static-{i} {padding}"),
                    ChunkSource::Static,
                    Some(CorpusType::Legal),
                )
            })
            .collect();
        let dynamics: Vec<SearchResult> = (1..=4)
            .map(|i| {
                make_result(
                    &format!("dynamic-{i} {padding}"),
                    ChunkSource::Dynamic,
                    None,
                )
            })
            .collect();

        let budget = 260;
        let ctx = ContextBuilder::build_context_bounded(&statics, &dynamics, "what is x?", budget);

        assert!(ContextBuilder::estimate_tokens(&ctx) <= budget);
        assert!(ctx.contains("=== 参考资料 ==="));
        assert!(ctx.contains("=== 问题 ==="));
        assert!(ctx.contains("what is x?"));
        assert!(ctx.contains("static-1"));
        assert!(ctx.contains("dynamic-1"));
        assert!(!ctx.contains("static-4"));
        assert!(!ctx.contains("dynamic-4"));
        // within-group order preserved
        let s1 = ctx.find("static-1").unwrap();
        let s2 = ctx.find("static-2").unwrap();
        assert!(s1 < s2);
    }

    #[test]
    fn estimate_matches_design_doc_k15_magnitude() {
        // Design doc §7: compliance review K=15 → 90 chunks ≈ 300 tokens each
        // ≈ 27k tokens total; a 9k budget (K=5 tier) must truncate below 9k.
        let text = "法".repeat(300);
        let statics: Vec<SearchResult> = (0..45)
            .map(|_| make_result(&text, ChunkSource::Static, Some(CorpusType::Legal)))
            .collect();
        let dynamics: Vec<SearchResult> = (0..45)
            .map(|_| make_result(&text, ChunkSource::Dynamic, None))
            .collect();

        let full = ContextBuilder::build_context(&statics, &dynamics, "查询");
        let total = ContextBuilder::estimate_tokens(&full);
        assert!((27_000..28_500).contains(&total), "total = {total}");

        let bounded = ContextBuilder::build_context_bounded(&statics, &dynamics, "查询", 9_000);
        assert!(ContextBuilder::estimate_tokens(&bounded) <= 9_000);
        assert!(bounded.contains("=== 参考资料 ==="));
        assert!(bounded.contains("=== 问题 ===\n查询"));
    }

    #[test]
    fn bounded_zero_budget_keeps_skeleton() {
        let static_r = make_result("law text", ChunkSource::Static, Some(CorpusType::Legal));
        let ctx = ContextBuilder::build_context_bounded(&[static_r], &[], "test?", 0);
        assert!(ctx.contains("=== 参考资料 ==="));
        assert!(ctx.contains("=== 问题 ===\ntest?"));
        assert!(!ctx.contains("law text"));
    }
}
