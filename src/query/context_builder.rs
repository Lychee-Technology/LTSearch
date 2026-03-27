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
            let label = corpus_type_label(r.corpus_type.as_ref());
            out.push_str(&format!("[{label} #{}]\n{}\n\n", i + 1, r.text));
        }

        for (i, r) in dynamic_chunks.iter().enumerate() {
            out.push_str(&format!("[用户数据 #{}]\n{}\n\n", i + 1, r.text));
        }

        out.push_str(&format!("=== 问题 ===\n{query}"));
        out
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
        Some(w) if w.static_bias > 0.7 => {
            "如法规/合同与用户数据冲突，以法规/合同为准。"
        }
        Some(w) if w.dynamic_bias > 0.7 => {
            "优先参考用户数据，不足时补充引用法规/合同。"
        }
        _ => "综合两类来源回答，不偏向任何一方。",
    }
}

fn corpus_type_label(ct: Option<&CorpusType>) -> &'static str {
    match ct {
        Some(CorpusType::Legal) => "法规",
        Some(CorpusType::Contract) => "合同",
        Some(CorpusType::Rfc) => "RFC",
        _ => "法规/合同",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChunkSource, SearchSource};

    fn make_result(text: &str, chunk_source: ChunkSource, ct: Option<CorpusType>) -> SearchResult {
        SearchResult {
            doc_id: "1".into(),
            score: 0.9,
            text: text.into(),
            metadata: None,
            source: SearchSource::Vector,
            chunk_source,
            corpus_type: ct,
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
}
