use crate::format::{GenerateContentResponse, MessagesResponse, convert_response};

pub fn parse_response(
    response: &GenerateContentResponse,
    model: &str,
    request_id: &str,
) -> MessagesResponse {
    convert_response(response, model, request_id)
}
