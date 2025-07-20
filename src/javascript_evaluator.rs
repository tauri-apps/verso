// Modified from https://github.com/servo/servo/blob/7ab0d9110913ae4e789ff60d10dbfa588717f914/components/servo/javascript_evaluator.rs

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;

use base::id::WebViewId;
use constellation_traits::EmbedderToConstellationMessage;
use crossbeam_channel::Sender;
use embedder_traits::{JSValue, JavaScriptEvaluationError, JavaScriptEvaluationId};

use crate::verso::send_to_constellation;

struct PendingEvaluation {
    callback: Box<dyn FnOnce(Result<JSValue, JavaScriptEvaluationError>)>,
}

pub struct JavaScriptEvaluator {
    current_id: JavaScriptEvaluationId,
    pending_evaluations: HashMap<JavaScriptEvaluationId, PendingEvaluation>,
}

impl JavaScriptEvaluator {
    pub fn new() -> Self {
        Self {
            current_id: JavaScriptEvaluationId(0),
            pending_evaluations: Default::default(),
        }
    }

    fn generate_id(&mut self) -> JavaScriptEvaluationId {
        let next_id = JavaScriptEvaluationId(self.current_id.0 + 1);
        std::mem::replace(&mut self.current_id, next_id)
    }

    pub fn evaluate(
        &mut self,
        constellation_sender: &Sender<EmbedderToConstellationMessage>,
        webview_id: &WebViewId,
        js: impl Into<String>,
        callback: Box<dyn FnOnce(Result<JSValue, JavaScriptEvaluationError>)>,
    ) {
        let evaluation_id = self.generate_id();
        send_to_constellation(
            constellation_sender,
            EmbedderToConstellationMessage::EvaluateJavaScript(
                *webview_id,
                evaluation_id,
                js.into(),
            ),
        );
        self.pending_evaluations
            .insert(evaluation_id, PendingEvaluation { callback });
    }

    pub fn evaluate_ignore_result(
        &mut self,
        constellation_sender: &Sender<EmbedderToConstellationMessage>,
        webview_id: &WebViewId,
        js: impl Into<String>,
    ) {
        self.evaluate(
            constellation_sender,
            webview_id,
            js.into(),
            Box::new(|_| {}),
        );
    }

    pub fn finish_evaluation(
        &mut self,
        evaluation_id: JavaScriptEvaluationId,
        result: Result<JSValue, JavaScriptEvaluationError>,
    ) {
        (self
            .pending_evaluations
            .remove(&evaluation_id)
            .expect("Received request to finish unknown JavaScript evaluation.")
            .callback)(result)
    }
}
