# Yêu cầu xuất suy luận

Trước JSON cuối, chỉ xuất đúng một khối `<why>...</why>`.

- Bắt đầu phản hồi ngay bằng `<why>` viết thường chính xác, không thuộc tính, và kết thúc bằng `</why>`.
- Nội dung sau Unicode trim phải khác rỗng và phân tích cho từng `[ID]`: ngữ cảnh, đối tượng, ngôi xưng,
  giọng điệu, thuật ngữ, xuống dòng, ATT token, phần ngôn ngữ nguồn còn sót và định dạng cuối.
- Không chỉ viết “đã kiểm tra” và không đặt JSON cuối bên trong `<why>`.
- Sau `</why>` chỉ được có khoảng trắng rồi đến JSON mà system Prompt yêu cầu.
