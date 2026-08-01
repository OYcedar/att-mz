# 示例

输入：

```json
{
  "terminology": [
    {
      "source": "ミストリア",
      "translation": "米斯特里亚"
    }
  ],
  "groups": [
    {
      "kind": "dialogue",
      "units": [
        {
          "role": "speaker",
          "text": ["村の老婆"]
        },
        {
          "id": "0",
          "role": "body",
          "type": "free",
          "text": [
            "おや、あんた、旅の人かい？",
            "しーっ……ミストリアの森じゃ、日が落ちると魔物がぞろぞろ出てくるんだよ。"
          ]
        },
        {
          "id": "1",
          "role": "choices",
          "type": "strict",
          "text": ["話を聞く", "", "宿へ戻る"]
        },
        {
          "id": "2",
          "role": "body",
          "type": "strict",
          "text": ["⟦ATT_ACTOR_NAME_WHOLE_0000⟧にも、早く戻るよう伝えておくれ。"]
        }
      ]
    }
  ]
}
```

输出：

```json
{
  "0": {
    "source": [
      "おや、あんた、旅の人かい？",
      "しーっ……ミストリアの森じゃ、日が落ちると魔物がぞろぞろ出てくるんだよ。"
    ],
    "translation": [
      "哎呀，你是外地来的旅人吧？",
      "嘘……米斯特里亚森林一到天黑，魔物就会成群结队地冒出来。"
    ]
  },
  "1": {
    "source": ["話を聞く", "", "宿へ戻る"],
    "translation": ["打听消息", "", "返回旅店"]
  },
  "2": {
    "source": ["⟦ATT_ACTOR_NAME_WHOLE_0000⟧にも、早く戻るよう伝えておくれ。"],
    "translation": ["也告诉⟦ATT_ACTOR_NAME_WHOLE_0000⟧一声，让他早点回来。"]
  }
}
```
