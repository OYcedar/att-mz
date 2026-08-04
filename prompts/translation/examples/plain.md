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
          "text": ["若い案内人"]
        },
        {
          "id": "0",
          "role": "body",
          "type": "free",
          "text": [
            "雨は、まだ止みそうにない。",
            "それでも夜明けまでには、",
            "ミストリアの峠を越えておきたいんだ。"
          ]
        },
        {
          "id": "1",
          "role": "body",
          "type": "free",
          "text": [
            "町に着いたら宿を探して、それから⟦ATT_ACTOR_NAME_WHOLE_0000⟧の行方を聞こう。"
          ]
        },
        {
          "id": "2",
          "role": "choices",
          "type": "strict",
          "text": ["急いで峠を越える", "", "村へ引き返す"]
        }
      ]
    }
  ]
}
```

输出：

```json
{
  "0": [
    "雨一时半会儿还停不了。",
    "可我还是想赶在天亮前翻过米斯特里亚山口。"
  ],
  "1": [
    "到了镇上，先找家旅店。",
    "然后再打听⟦ATT_ACTOR_NAME_WHOLE_0000⟧的下落。"
  ],
  "2": ["尽快翻过山口", "", "返回村庄"]
}
```
