use std::str::CharIndices;

use super::{Token, TokenStream, Tokenizer};

/// Tokenize the text by splitting on whitespaces and punctuation.
#[derive(Clone, Default)]
pub struct SlashTokenizer {
    token: Token,
}

/// TokenStream produced by the `SlashTokenizer`.
pub struct SlashTokenStream<'a> {
    text: &'a str,
    chars: CharIndices<'a>,
    token: &'a mut Token,
}

impl Tokenizer for SlashTokenizer {
    type TokenStream<'a> = SlashTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> SlashTokenStream<'a> {
        self.token.reset();
        SlashTokenStream {
            text,
            chars: text.char_indices(),
            token: &mut self.token,
        }
    }
}

impl<'a> SlashTokenStream<'a> {
    // search for the end of the current token.
    fn search_token_end(&mut self) -> usize {
        (&mut self.chars)
            .filter(|(_, c)| *c=='/')
            .map(|(offset, _)| offset)
            .next()
            .unwrap_or(self.text.len())
    }
}

impl<'a> TokenStream for SlashTokenStream<'a> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);
        while let Some((offset_from, c)) = self.chars.next() {
            if c!='/' {
                let offset_to = self.search_token_end();
                self.token.offset_from = offset_from;
                self.token.offset_to = offset_to;
                self.token.text.push_str(&self.text[offset_from..offset_to]);
                return true;
            }
        }
        false
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

#[cfg(test)]
mod tests {
    use crate::tokenizer::tests::assert_token;
    use crate::tokenizer::{SlashTokenizer, TextAnalyzer, Token};

    #[test]
    fn test_simple_tokenizer() {
        let tokens = token_stream_helper("/home/mochix/.subversion/auth");
        assert_eq!(tokens.len(), 4);
        assert_token(&tokens[0], 0, "home", 1, 5);
        assert_token(&tokens[1], 1, "mochix", 6, 12);
        assert_token(&tokens[2], 2, ".subversion", 13, 24);
        assert_token(&tokens[3], 3, "auth", 25, 29);
    }

    fn token_stream_helper(text: &str) -> Vec<Token> {
        let mut a = TextAnalyzer::from(SlashTokenizer::default());
        let mut token_stream = a.token_stream(text);
        let mut tokens: Vec<Token> = vec![];
        let mut add_token = |token: &Token| {
            tokens.push(token.clone());
        };
        token_stream.process(&mut add_token);
        tokens
    }
}
